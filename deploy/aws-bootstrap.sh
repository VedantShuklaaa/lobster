#!/usr/bin/env bash
# One-time AWS side of the SSM deploy. Run it in AWS CloudShell (already logged in as you), region ap-south-1:
#
#   bash aws-bootstrap.sh
#
# Creates: a private S3 bucket for release tarballs, GitHub's OIDC identity provider, a role that only
# this repo's main branch can assume (upload to the bucket, run one SSM document on one instance), and an
# instance role so the box can talk to SSM. Also turns IMDSv2 on. Safe to run again.
set -euo pipefail

REGION=${REGION:-ap-south-1}
REPO=${REPO:-VedantShuklaaa/lobster}
EIP=${EIP:-3.108.71.220}
export AWS_DEFAULT_REGION=$REGION

acct=$(aws sts get-caller-identity --query Account --output text)
iid=$(aws ec2 describe-instances \
  --filters "Name=ip-address,Values=$EIP" "Name=instance-state-name,Values=running" \
  --query 'Reservations[0].Instances[0].InstanceId' --output text)
[ "$iid" != None ] && [ -n "$iid" ] || { echo "no running instance with $EIP in $REGION"; exit 1; }
bucket=lobster-releases-$acct
echo "account $acct, instance $iid, bucket $bucket"

# 1. bucket: private, releases expire after a week
if ! aws s3api head-bucket --bucket "$bucket" 2>/dev/null; then
  aws s3api create-bucket --bucket "$bucket" \
    --create-bucket-configuration "LocationConstraint=$REGION" >/dev/null
fi
aws s3api put-public-access-block --bucket "$bucket" --public-access-block-configuration \
  BlockPublicAcls=true,IgnorePublicAcls=true,BlockPublicPolicy=true,RestrictPublicBuckets=true
aws s3api put-bucket-lifecycle-configuration --bucket "$bucket" --lifecycle-configuration \
  '{"Rules":[{"ID":"expire-releases","Status":"Enabled","Filter":{"Prefix":"releases/"},"Expiration":{"Days":7}}]}'

# 2. GitHub as an identity provider
oidc=arn:aws:iam::$acct:oidc-provider/token.actions.githubusercontent.com
if ! aws iam get-open-id-connect-provider --open-id-connect-provider-arn "$oidc" >/dev/null 2>&1; then
  aws iam create-open-id-connect-provider --url https://token.actions.githubusercontent.com \
    --client-id-list sts.amazonaws.com \
    --thumbprint-list 6938fd4d98bab03faadb97b34396831e3780aea1 >/dev/null
fi

# 3. the role CD assumes: only this repo, only the main branch
trust=$(cat <<JSON
{"Version":"2012-10-17","Statement":[{"Effect":"Allow","Principal":{"Federated":"$oidc"},
 "Action":"sts:AssumeRoleWithWebIdentity",
 "Condition":{"StringEquals":{"token.actions.githubusercontent.com:aud":"sts.amazonaws.com",
    "token.actions.githubusercontent.com:sub":"repo:VedantShuklaaa@102453727/lobster@1403710487:ref:refs/heads/main"}}}]}
JSON
)
role=lobster-github-deploy
if aws iam get-role --role-name "$role" >/dev/null 2>&1; then
  aws iam update-assume-role-policy --role-name "$role" --policy-document "$trust"
else
  aws iam create-role --role-name "$role" --assume-role-policy-document "$trust" >/dev/null
fi
policy=$(cat <<JSON
{"Version":"2012-10-17","Statement":[
 {"Effect":"Allow","Action":["s3:PutObject","s3:GetObject"],"Resource":"arn:aws:s3:::$bucket/releases/*"},
 {"Effect":"Allow","Action":"ssm:SendCommand","Resource":[
   "arn:aws:ec2:$REGION:$acct:instance/$iid","arn:aws:ssm:$REGION::document/AWS-RunShellScript"]},
 {"Effect":"Allow","Action":"ssm:GetCommandInvocation","Resource":"*"}]}
JSON
)
aws iam put-role-policy --role-name "$role" --policy-name deploy --policy-document "$policy"

# 4. the instance's own role, so the SSM agent on the box can register
erole=lobster-ec2-ssm
etrust='{"Version":"2012-10-17","Statement":[{"Effect":"Allow","Principal":{"Service":"ec2.amazonaws.com"},"Action":"sts:AssumeRole"}]}'
aws iam get-role --role-name "$erole" >/dev/null 2>&1 \
  || aws iam create-role --role-name "$erole" --assume-role-policy-document "$etrust" >/dev/null
aws iam attach-role-policy --role-name "$erole" \
  --policy-arn arn:aws:iam::aws:policy/AmazonSSMManagedInstanceCore
aws iam get-instance-profile --instance-profile-name "$erole" >/dev/null 2>&1 || {
  aws iam create-instance-profile --instance-profile-name "$erole" >/dev/null
  aws iam add-role-to-instance-profile --instance-profile-name "$erole" --role-name "$erole"
}
have=$(aws ec2 describe-iam-instance-profile-associations --filters "Name=instance-id,Values=$iid" \
  --query 'IamInstanceProfileAssociations[0].IamInstanceProfile.Arn' --output text)
if [ "$have" = None ]; then
  sleep 10   # IAM is eventually consistent
  aws ec2 associate-iam-instance-profile --instance-id "$iid" \
    --iam-instance-profile "Name=$erole" >/dev/null
elif [[ $have != *"/$erole" ]]; then
  echo "instance already has the profile $have: attach AmazonSSMManagedInstanceCore to that role instead"
fi

# 5. IMDSv2 only (blocks the classic SSRF-to-credentials trick)
aws ec2 modify-instance-metadata-options --instance-id "$iid" \
  --http-tokens required --http-endpoint enabled --http-put-response-hop-limit 1 >/dev/null

cat <<OUT

Add these as GitHub repo variables (Settings > Secrets and variables > Actions > Variables):
  AWS_ROLE_ARN      arn:aws:iam::$acct:role/$role
  AWS_REGION        $REGION
  DEPLOY_BUCKET     $bucket
  EC2_INSTANCE_ID   $iid

The instance shows up under Systems Manager > Fleet Manager within a few minutes.
OUT
