---
title: "Object storage"
description: "Query Parquet, CSV and JSON files in S3, Google Cloud Storage, Azure Blob Storage and over HTTPS."
---

Anything that works with a local path works with a URL. Credentials come from the standard environment variables (`AWS_*`, `GOOGLE_*`, `AZURE_*`) or the usual config files (see [Credentials](#credentials) below):

<!-- skip -->
```sql
CREATE VIEW follows AS
  SELECT * FROM 's3://my-bucket/social/follows/*.parquet';

CREATE PROPERTY GRAPH social
  EDGE TABLES (follows SOURCE KEY (follower) DESTINATION KEY (followee));

SELECT * FROM pagerank(social) ORDER BY pagerank DESC LIMIT 10;
```

`gs://`, `az://` and `https://` work the same way. Ursa reads only the columns a query needs, and stores the graph index in the database file, so the second query does not read the edges again.

## Credentials

Ursa finds credentials where each cloud's own tools do. It looks in the places below in order, and uses the first that has credentials. Nothing is stored in the database file.

**S3** (`s3://`):

1. Environment variables: `AWS_ACCESS_KEY_ID` and `AWS_SECRET_ACCESS_KEY`, plus `AWS_SESSION_TOKEN` for temporary credentials.
2. An AWS profile in `~/.aws/credentials` and `~/.aws/config`. Ursa reads the profile that `AWS_PROFILE` names, or `default`. The profile can hold:
    - access keys;
    - an SSO login (run `aws sso login` first);
    - a role to assume (`role_arn` with `source_profile`);
    - a `credential_process`.

    `AWS_SHARED_CREDENTIALS_FILE` and `AWS_CONFIG_FILE` point to other files. If `AWS_PROFILE` names a profile that has no credentials, the query fails. Ursa doesn't go on to the sources below.
3. Web identity: `AWS_WEB_IDENTITY_TOKEN_FILE` and `AWS_ROLE_ARN`, as on EKS.
4. Container credentials: `AWS_CONTAINER_CREDENTIALS_RELATIVE_URI` or `AWS_CONTAINER_CREDENTIALS_FULL_URI`, as on ECS.
5. The instance's role, from the EC2 instance metadata service.

```bash
AWS_PROFILE=analytics ursa -c "SELECT count(*) FROM 's3://my-bucket/follows/*.parquet'"
```

The region comes from `AWS_REGION` or `AWS_DEFAULT_REGION`. If neither is set, Ursa asks S3 where the bucket is. If S3 doesn't answer, Ursa uses the profile's `region`.

Some settings for other cases:

- **An S3-compatible store** (MinIO, R2): set `AWS_ENDPOINT`. For a plain `http://` endpoint, also set `AWS_ALLOW_HTTP=true`.
- **A public bucket**: set `AWS_SKIP_SIGNATURE=true` and no credentials are needed.

Ursa reads profiles through `aws-config`, the AWS SDK's own configuration loader, in the `aws-profile` feature of `ursa-cli`. The feature is on by default. A build without it (`--no-default-features`) skips step 2.

**Google Cloud Storage** (`gs://`):

1. A service account key: a file named by `GOOGLE_SERVICE_ACCOUNT` (or `GOOGLE_SERVICE_ACCOUNT_PATH`), or the key's JSON in `GOOGLE_SERVICE_ACCOUNT_KEY`.
2. Application default credentials: the file `GOOGLE_APPLICATION_CREDENTIALS` names, or the one `gcloud auth application-default login` writes to `~/.config/gcloud/`.
3. The instance's service account, from the GCE metadata server.

**Azure Blob Storage** (`az://`, `abfs://`, `abfss://`, `adl://`). Set the account with `AZURE_STORAGE_ACCOUNT_NAME`, then the credentials:

1. An account key: `AZURE_STORAGE_ACCOUNT_KEY`.
2. Workload identity: `AZURE_CLIENT_ID`, `AZURE_TENANT_ID` and `AZURE_FEDERATED_TOKEN_FILE`, as on AKS.
3. A service principal: `AZURE_CLIENT_ID`, `AZURE_CLIENT_SECRET` and `AZURE_TENANT_ID`.
4. A SAS token: `AZURE_STORAGE_SAS_KEY`.
5. The Azure CLI's login, when `AZURE_USE_AZURE_CLI=true`.
6. A managed identity, from the instance metadata service.

**HTTP** (`https://`, `http://`) URLs are read without credentials.
