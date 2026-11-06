//! S3 credentials from AWS profile files (#264): `AWS_PROFILE=x ursa -c
//! "SELECT * FROM 's3://…'"` signs its requests with profile `x`'s keys.
//!
//! The bucket is a fake S3 on localhost (path-style, plain HTTP), reached
//! through `AWS_ENDPOINT`. It serves one CSV file, and answers a request only
//! when it is signed with the access key the test expects, so a query that
//! returns rows was signed with that key. Each run gets a fresh environment
//! (`env_clear`), so nothing from the machine running the tests leaks in.
#![cfg(feature = "aws-profile")]

use std::io::{BufRead, BufReader, Write};
use std::net::{TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};
use std::sync::{Arc, Mutex};

const BUCKET: &str = "bucket";
const KEY: &str = "karate_edges.csv";

fn karate_edges() -> Vec<u8> {
    let path = Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .unwrap()
        .join("python/ursa/datasets/data/karate_edges.csv");
    std::fs::read(path).unwrap()
}

/// A fake S3 that serves `KEY` in `BUCKET` to requests signed with `key_id`.
struct FakeS3 {
    endpoint: String,
    /// The `Authorization` header of every request.
    signatures: Arc<Mutex<Vec<String>>>,
}

impl FakeS3 {
    fn start(key_id: &'static str) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let endpoint = format!("http://{}", listener.local_addr().unwrap());
        let signatures = Arc::new(Mutex::new(Vec::new()));
        let seen = Arc::clone(&signatures);
        let body = karate_edges();
        std::thread::spawn(move || {
            for stream in listener.incoming().flatten() {
                serve(stream, key_id, &body, &seen);
            }
        });
        Self {
            endpoint,
            signatures,
        }
    }

    /// The `Authorization` headers seen so far.
    fn signatures(&self) -> Vec<String> {
        self.signatures.lock().unwrap().clone()
    }
}

/// Answer one request on `stream` (each response closes the connection).
fn serve(mut stream: TcpStream, key_id: &str, body: &[u8], seen: &Mutex<Vec<String>>) {
    let mut reader = BufReader::new(stream.try_clone().unwrap());
    let mut request_line = String::new();
    if reader.read_line(&mut request_line).is_err() {
        return;
    }
    let mut authorization = String::new();
    let mut range: Option<(usize, usize)> = None;
    loop {
        let mut line = String::new();
        if reader.read_line(&mut line).unwrap_or(0) == 0 || line == "\r\n" {
            break;
        }
        let (name, value) = line.split_once(':').unwrap_or((&line, ""));
        let value = value.trim();
        match name.to_ascii_lowercase().as_str() {
            "authorization" => authorization = value.to_string(),
            "range" => {
                range = value
                    .strip_prefix("bytes=")
                    .and_then(|r| r.split_once('-'))
                    .map(|(a, b)| {
                        let start = a.parse().unwrap();
                        let end = b.parse::<usize>().map_or(body.len(), |e| e + 1);
                        (start, end.min(body.len()))
                    })
            }
            _ => {}
        }
    }
    seen.lock().unwrap().push(authorization.clone());
    let mut parts = request_line.split_whitespace();
    let method = parts.next().unwrap_or("");
    let target = parts.next().unwrap_or("");
    let (path, query) = target.split_once('?').unwrap_or((target, ""));

    let head = method == "HEAD";
    if !authorization.contains(&format!("Credential={key_id}/")) {
        return xml(
            &mut stream,
            "403 Forbidden",
            "<Error><Code>InvalidAccessKeyId</Code><Message>not the expected key</Message>\
             </Error>"
                .to_string(),
        );
    }
    let object = format!("/{BUCKET}/{KEY}");
    let etag = "\"0123456789abcdef\"".to_string();
    if path.trim_end_matches('/') == format!("/{BUCKET}") && query.contains("list-type=2") {
        let prefix = query
            .split('&')
            .find_map(|kv| kv.strip_prefix("prefix="))
            .unwrap_or("");
        let contents = if KEY.starts_with(prefix) {
            format!(
                "<Contents><Key>{KEY}</Key><LastModified>2024-01-01T00:00:00.000Z</LastModified>\
                 <ETag>{etag}</ETag><Size>{}</Size><StorageClass>STANDARD</StorageClass>\
                 </Contents>",
                body.len()
            )
        } else {
            String::new()
        };
        return xml(
            &mut stream,
            "200 OK",
            format!(
                "<?xml version=\"1.0\" encoding=\"UTF-8\"?><ListBucketResult><Name>{BUCKET}</Name>\
                 <Prefix>{prefix}</Prefix><MaxKeys>1000</MaxKeys><IsTruncated>false</IsTruncated>\
                 {contents}</ListBucketResult>"
            ),
        );
    }
    if path == object {
        let meta = |len: usize| {
            vec![
                ("Content-Length", len.to_string()),
                ("Last-Modified", "Mon, 01 Jan 2024 00:00:00 GMT".to_string()),
                ("ETag", etag.clone()),
            ]
        };
        return match range {
            Some((start, end)) if method == "GET" => {
                let mut headers = meta(end - start);
                headers.push((
                    "Content-Range",
                    format!("bytes {start}-{}/{}", end - 1, body.len()),
                ));
                reply(
                    &mut stream,
                    head,
                    "206 Partial Content",
                    &headers,
                    &body[start..end],
                )
            }
            _ => reply(&mut stream, head, "200 OK", &meta(body.len()), body),
        };
    }
    xml(
        &mut stream,
        "404 Not Found",
        "<Error><Code>NoSuchKey</Code><Message>no such key</Message></Error>".to_string(),
    )
}

/// Write a response with `headers`, and `payload` unless it answers a HEAD.
fn reply(
    stream: &mut TcpStream,
    head: bool,
    status: &str,
    headers: &[(&str, String)],
    payload: &[u8],
) {
    let mut text = format!("HTTP/1.1 {status}\r\nConnection: close\r\n");
    for (name, value) in headers {
        text.push_str(&format!("{name}: {value}\r\n"));
    }
    text.push_str("\r\n");
    let mut out = text.into_bytes();
    if !head {
        out.extend_from_slice(payload);
    }
    let _ = stream.write_all(&out);
}

/// An XML response.
fn xml(stream: &mut TcpStream, status: &str, text: String) {
    let headers = [
        ("Content-Type", "application/xml".to_string()),
        ("Content-Length", text.len().to_string()),
    ];
    reply(stream, false, status, &headers, text.as_bytes());
}

/// A home directory holding `.aws/credentials` and `.aws/config`.
fn aws_home(credentials: &str, config: &str) -> tempfile::TempDir {
    let home = tempfile::tempdir().unwrap();
    let aws = home.path().join(".aws");
    std::fs::create_dir(&aws).unwrap();
    std::fs::write(aws.join("credentials"), credentials).unwrap();
    std::fs::write(aws.join("config"), config).unwrap();
    home
}

/// `ursa -c <sql>` with only `HOME`, the fake S3's endpoint and `env` set.
fn ursa(home: &Path, s3: &FakeS3, env: &[(&str, &str)], sql: &str) -> Output {
    let mut command = Command::new(env!("CARGO_BIN_EXE_ursa"));
    command
        .args(["-f", "csv", "-c", sql])
        .env_clear()
        .env("HOME", home)
        .env("AWS_ENDPOINT", &s3.endpoint)
        .env("AWS_ALLOW_HTTP", "true")
        .stdin(Stdio::null());
    for (k, v) in env {
        command.env(k, v);
    }
    command.output().expect("ursa runs")
}

const COUNT: &str = "SELECT count(*) AS edges FROM 's3://bucket/karate_edges.csv'";

fn counted(output: Output) -> String {
    assert!(
        output.status.success(),
        "ursa failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8(output.stdout).unwrap()
}

#[test]
fn a_named_profile_signs_the_requests() {
    let s3 = FakeS3::start("AKIDPROFILE");
    let home = aws_home(
        "[default]\naws_access_key_id = AKIDDEFAULT\naws_secret_access_key = default-secret\n\n\
         [analytics]\naws_access_key_id = AKIDPROFILE\naws_secret_access_key = profile-secret\n",
        "[profile analytics]\nregion = eu-west-3\n",
    );
    let out = ursa(home.path(), &s3, &[("AWS_PROFILE", "analytics")], COUNT);
    assert_eq!(counted(out), "edges\n78\n");
    // Signed with the profile's key, for the profile's region.
    let signatures = s3.signatures();
    assert!(!signatures.is_empty());
    for s in signatures {
        assert!(
            s.contains("Credential=AKIDPROFILE/") && s.contains("/eu-west-3/s3/"),
            "{s}"
        );
    }
}

#[test]
fn the_default_profile_is_read_without_aws_profile() {
    let s3 = FakeS3::start("AKIDDEFAULT");
    let home = aws_home(
        "[default]\naws_access_key_id = AKIDDEFAULT\naws_secret_access_key = default-secret\n",
        "",
    );
    assert_eq!(counted(ursa(home.path(), &s3, &[], COUNT)), "edges\n78\n");
}

#[test]
fn files_named_by_the_environment_are_read() {
    let s3 = FakeS3::start("AKIDELSEWHERE");
    let home = aws_home("", "");
    let dir = tempfile::tempdir().unwrap();
    let credentials: PathBuf = dir.path().join("creds");
    std::fs::write(
        &credentials,
        "[ci]\naws_access_key_id = AKIDELSEWHERE\naws_secret_access_key = s\n",
    )
    .unwrap();
    let out = ursa(
        home.path(),
        &s3,
        &[
            ("AWS_PROFILE", "ci"),
            ("AWS_SHARED_CREDENTIALS_FILE", credentials.to_str().unwrap()),
        ],
        COUNT,
    );
    assert_eq!(counted(out), "edges\n78\n");
}

#[test]
fn environment_keys_win_over_the_profile() {
    let s3 = FakeS3::start("AKIDENV");
    let home = aws_home(
        "[default]\naws_access_key_id = AKIDDEFAULT\naws_secret_access_key = default-secret\n",
        "",
    );
    let out = ursa(
        home.path(),
        &s3,
        &[
            ("AWS_ACCESS_KEY_ID", "AKIDENV"),
            ("AWS_SECRET_ACCESS_KEY", "env-secret"),
        ],
        COUNT,
    );
    assert_eq!(counted(out), "edges\n78\n");
}

#[cfg(unix)]
#[test]
fn a_credential_process_is_run() {
    let s3 = FakeS3::start("AKIDPROCESS");
    let dir = tempfile::tempdir().unwrap();
    let json = dir.path().join("credentials.json");
    std::fs::write(
        &json,
        r#"{"Version": 1, "AccessKeyId": "AKIDPROCESS", "SecretAccessKey": "process-secret"}"#,
    )
    .unwrap();
    let home = aws_home(
        "",
        &format!(
            "[profile tool]\ncredential_process = cat {}\n",
            json.display()
        ),
    );
    let out = ursa(
        home.path(),
        &s3,
        &[("AWS_PROFILE", "tool"), ("PATH", "/usr/bin:/bin")],
        COUNT,
    );
    assert_eq!(counted(out), "edges\n78\n");
}

#[test]
fn a_named_profile_that_gives_no_credentials_is_an_error() {
    let s3 = FakeS3::start("AKIDDEFAULT");
    let home = aws_home(
        "[default]\naws_access_key_id = AKIDDEFAULT\naws_secret_access_key = s\n",
        "",
    );
    let out = ursa(home.path(), &s3, &[("AWS_PROFILE", "nowhere")], COUNT);
    assert!(!out.status.success());
    let stderr = String::from_utf8(out.stderr).unwrap();
    assert!(
        stderr
            .contains("cannot connect to s3://bucket: AWS profile 'nowhere' gives no credentials"),
        "{stderr}"
    );
    // Nothing was sent with other credentials.
    assert!(s3.signatures().is_empty());
}

#[test]
fn a_wrong_key_is_refused_by_the_bucket() {
    // The fake bucket really checks the key: a profile with another key fails.
    let s3 = FakeS3::start("AKIDEXPECTED");
    let home = aws_home(
        "[default]\naws_access_key_id = AKIDOTHER\naws_secret_access_key = s\n",
        "",
    );
    let out = ursa(home.path(), &s3, &[], COUNT);
    assert!(!out.status.success());
}
