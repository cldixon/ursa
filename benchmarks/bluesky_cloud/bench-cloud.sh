#!/usr/bin/env bash
# Operate the Bluesky benchmark in AWS. See README.md.
#
#   bench-cloud.sh deploy                 create or update the stack (stack.yml)
#   bench-cloud.sh push [VERSION]         build the arm64 image with ursa VERSION, push to ECR
#   bench-cloud.sh upload-data DIR        copy the two data files from DIR to the bucket
#   bench-cloud.sh run [run.py args...]   run the benchmark as a Fargate task and wait
#   bench-cloud.sh results [RUN_ID]       download a run's results (default: the newest)
#   bench-cloud.sh ec2-up [TYPE] [spot]   start an instance for live work (default r7g.4xlarge)
#   bench-cloud.sh ec2-connect            open a shell on it (SSM Session Manager)
#   bench-cloud.sh ec2-down               terminate it
#   bench-cloud.sh outputs                print the stack's outputs
#
# Settings (environment):
#   AWS_PROFILE      required: the profile to use. Nothing falls back to a default.
#   BENCH_ACCOUNT    required: the AWS account id you expect. Every command checks
#                    that AWS_PROFILE signs in to it, so a wrong profile never
#                    deploys or spends anything.
#   AWS_REGION       default us-east-1
#   BENCH_STACK      default ursa-bluesky-bench
#   URSA_VERSION     default 0.9.0 (push, and the run's image tag)
#   BENCH_RUN_ID     run: the results folder name (default: a UTC timestamp + version)
#   BENCH_STARTED_BY run: the task's startedBy tag (default bench-cloud)
set -euo pipefail

here=$(cd "$(dirname "$0")" && pwd)
# A named profile, or credentials already in the environment (GitHub Actions'
# OIDC role); never whatever the default profile happens to be.
if [ -z "${AWS_ACCESS_KEY_ID:-}" ]; then
    : "${AWS_PROFILE:?set AWS_PROFILE to the profile to use}"
    export AWS_PROFILE
fi
: "${BENCH_ACCOUNT:?set BENCH_ACCOUNT to the AWS account id you expect}"
export AWS_REGION=${AWS_REGION:-us-east-1}
export AWS_PAGER=""
STACK=${BENCH_STACK:-ursa-bluesky-bench}
URSA_VERSION=${URSA_VERSION:-0.9.0}

die() { echo "bench-cloud: $*" >&2; exit 1; }
say() { echo "bench-cloud: $*" >&2; }

check_account() {
    local acct
    acct=$(aws sts get-caller-identity --query Account --output text) \
        || die "could not sign in (AWS_PROFILE=${AWS_PROFILE:-unset})"
    [ "$acct" = "$BENCH_ACCOUNT" ] \
        || die "these credentials (AWS_PROFILE=${AWS_PROFILE:-unset}) are account $acct, not BENCH_ACCOUNT=$BENCH_ACCOUNT; stopping"
}

out() { # out NAME: one stack output
    aws cloudformation describe-stacks --stack-name "$STACK" \
        --query "Stacks[0].Outputs[?OutputKey=='$1'].OutputValue" --output text
}

cmd_deploy() {
    # ECS needs its service-linked role to create a cluster with capacity
    # providers. An account that has never used ECS lacks it, and the stack
    # then fails at the cluster; create it first (an error means it exists).
    aws iam create-service-linked-role --aws-service-name ecs.amazonaws.com >/dev/null 2>&1 || true
    aws cloudformation deploy --stack-name "$STACK" --template-file "$here/stack.yml" \
        --capabilities CAPABILITY_IAM --no-fail-on-empty-changeset \
        --parameter-overrides "ImageTag=ursa-$URSA_VERSION" "$@"
    cmd_outputs
}

cmd_outputs() {
    aws cloudformation describe-stacks --stack-name "$STACK" \
        --query 'Stacks[0].Outputs[].[OutputKey,OutputValue]' --output table
}

cmd_push() {
    local version=${1:-$URSA_VERSION} repo
    repo=$(out RepositoryUri)
    aws ecr get-login-password | docker login --username AWS --password-stdin "${repo%%/*}"
    docker build --platform linux/arm64 --build-arg "URSA_VERSION=$version" \
        -t "$repo:ursa-$version" "$here"
    docker push "$repo:ursa-$version"
    say "pushed $repo:ursa-$version"
}

cmd_upload_data() {
    local dir=${1:?usage: upload-data DIR} bucket
    bucket=$(out Bucket)
    for f in deidentified_follows_edgelist.parquet deidentified_nodes.csv.gz; do
        [ -f "$dir/$f" ] || die "$dir/$f not found"
        aws s3 cp "$dir/$f" "s3://$bucket/data/$f"
    done
}

# Register a task definition revision that runs ursa-$URSA_VERSION, print its ARN.
task_definition() {
    local family repo td
    family=$(out TaskFamily)
    repo=$(out RepositoryUri)
    td=$(aws ecs describe-task-definition --task-definition "$family" --query taskDefinition)
    python3 -I -c '
import json, sys
td = json.loads(sys.argv[1])
td["containerDefinitions"][0]["image"] = sys.argv[2]
keep = ("family", "taskRoleArn", "executionRoleArn", "networkMode", "containerDefinitions",
        "requiresCompatibilities", "cpu", "memory", "ephemeralStorage", "runtimePlatform")
print(json.dumps({k: td[k] for k in keep if k in td}))
' "$td" "$repo:ursa-$URSA_VERSION" > "${TMPDIR:-/tmp}/bench-td.json"
    aws ecs register-task-definition --cli-input-json "file://${TMPDIR:-/tmp}/bench-td.json" \
        --query taskDefinition.taskDefinitionArn --output text
}

cmd_run() {
    local bucket run_id td task overrides status code
    bucket=$(out Bucket)
    run_id=${BENCH_RUN_ID:-$(date -u +%Y%m%dT%H%M%SZ)-ursa-$URSA_VERSION}
    td=$(task_definition)
    overrides=$(python3 -I -c '
import json, sys
bucket, run_id, extra = sys.argv[1], sys.argv[2], sys.argv[3:]
cmd = ["--data", f"s3://{bucket}/data", "--work", "/work/run",
       "--results", f"s3://{bucket}/results/{run_id}"] + extra
print(json.dumps({"containerOverrides": [{"name": "bench", "command": cmd}]}))
' "$bucket" "$run_id" "$@")
    task=$(aws ecs run-task --cluster "$(out Cluster)" --task-definition "$td" \
        --launch-type FARGATE --count 1 --started-by "${BENCH_STARTED_BY:-bench-cloud}" \
        --network-configuration "awsvpcConfiguration={subnets=[$(out Subnets)],securityGroups=[$(out SecurityGroup)],assignPublicIp=ENABLED}" \
        --overrides "$overrides" --query 'tasks[0].taskArn' --output text)
    [ -n "$task" ] && [ "$task" != None ] || die "run-task started no task"
    say "started $task (run $run_id); results go to s3://$bucket/results/$run_id/"
    trap 'say "stopping $task"; aws ecs stop-task --cluster "$(out Cluster)" --task "$task" >/dev/null; exit 130' INT TERM
    while :; do
        status=$(aws ecs describe-tasks --cluster "$(out Cluster)" --tasks "$task" \
            --query 'tasks[0].lastStatus' --output text)
        say "$(date -u +%H:%M:%S) $status"
        [ "$status" = STOPPED ] && break
        sleep 30
    done
    trap - INT TERM
    code=$(aws ecs describe-tasks --cluster "$(out Cluster)" --tasks "$task" \
        --query 'tasks[0].[containers[0].exitCode,stoppedReason]' --output text)
    say "stopped: $code"
    cmd_results "$run_id" || true
    case "$code" in 0*) ;; *) exit 1 ;; esac
}

cmd_results() {
    local bucket run_id=${1:-} dest
    bucket=$(out Bucket)
    if [ -z "$run_id" ]; then
        run_id=$(aws s3api list-objects-v2 --bucket "$bucket" --prefix results/ --delimiter / \
            --query 'CommonPrefixes[].Prefix' --output text | tr '\t' '\n' | sort | tail -1)
        run_id=${run_id#results/}; run_id=${run_id%/}
    fi
    [ -n "$run_id" ] || die "no runs in s3://$bucket/results/"
    dest="$here/results/$run_id"
    mkdir -p "$dest"
    aws s3 cp --recursive --only-show-errors "s3://$bucket/results/$run_id/" "$dest/"
    [ -f "$dest/summary.md" ] && cat "$dest/summary.md"
    say "results in $dest"
}

instance_id() {
    aws ec2 describe-instances \
        --filters "Name=tag:ursa-bench,Values=$STACK" "Name=instance-state-name,Values=pending,running,stopping,stopped" \
        --query 'Reservations[].Instances[0].InstanceId' --output text
}

cmd_ec2_up() {
    local type=${1:-r7g.4xlarge} market=${2:-} ami subnet existing spot=()
    existing=$(instance_id)
    [ -z "$existing" ] || [ "$existing" = None ] || die "instance $existing is already up (ec2-down first)"
    ami=$(aws ssm get-parameter --name /aws/service/ami-amazon-linux-latest/al2023-ami-kernel-default-arm64 \
        --query Parameter.Value --output text)
    subnet=$(out Subnets); subnet=${subnet%%,*}
    [ "$market" = spot ] && spot=(--instance-market-options 'MarketType=spot,SpotOptions={SpotInstanceType=one-time,InstanceInterruptionBehavior=terminate}')
    # A gp3 volume for /work (data + stored indexes), Docker, and the image.
    aws ec2 run-instances --image-id "$ami" --instance-type "$type" --count 1 \
        --subnet-id "$subnet" --security-group-ids "$(out SecurityGroup)" \
        --iam-instance-profile "Name=$(out InstanceProfile)" \
        --metadata-options HttpTokens=required \
        --instance-initiated-shutdown-behavior terminate \
        --block-device-mappings 'DeviceName=/dev/xvda,Ebs={VolumeSize=250,VolumeType=gp3,Throughput=500,Iops=6000,DeleteOnTermination=true}' \
        --tag-specifications "ResourceType=instance,Tags=[{Key=ursa-bench,Value=$STACK},{Key=Name,Value=$STACK-live}]" \
        --user-data "$(sed -e "s|@REPO@|$(out RepositoryUri)|g" -e "s|@BUCKET@|$(out Bucket)|g" \
            -e "s|@VERSION@|$URSA_VERSION|g" -e "s|@REGION@|$AWS_REGION|g" "$here/ec2-user-data.sh")" \
        ${spot[@]+"${spot[@]}"} --query 'Instances[0].[InstanceId,InstanceType]' --output text
    say "starting; give it ~2 minutes, then: bench-cloud.sh ec2-connect"
    say "it costs money every hour it runs: bench-cloud.sh ec2-down when done"
}

cmd_ec2_connect() {
    local id
    id=$(instance_id)
    [ -n "$id" ] && [ "$id" != None ] || die "no instance is up"
    aws ssm start-session --target "$id"
}

cmd_ec2_down() {
    local id
    id=$(instance_id)
    [ -n "$id" ] && [ "$id" != None ] || { say "no instance is up"; return; }
    aws ec2 terminate-instances --instance-ids "$id" --query 'TerminatingInstances[0].CurrentState.Name' --output text
}

sub=${1:-}
[ -n "$sub" ] || { sed -n '2,22p' "$0"; exit 2; }
shift
check_account
case "$sub" in
    deploy) cmd_deploy "$@" ;;
    outputs) cmd_outputs ;;
    push) cmd_push "$@" ;;
    upload-data) cmd_upload_data "$@" ;;
    run) cmd_run "$@" ;;
    results) cmd_results "$@" ;;
    ec2-up) cmd_ec2_up "$@" ;;
    ec2-connect) cmd_ec2_connect ;;
    ec2-down) cmd_ec2_down ;;
    *) die "unknown command '$sub'" ;;
esac
