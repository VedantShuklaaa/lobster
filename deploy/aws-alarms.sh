#!/usr/bin/env bash
# One-time monitoring and cost guard. Run in AWS CloudShell, region ap-south-1:
#
#   EMAIL=you@example.com bash aws-alarms.sh
#
# Creates an SNS topic that emails you, a CloudWatch alarm for sustained high CPU (a flood or a runaway
# process), an alarm for failed instance status checks, and a monthly AWS Budget with 80% / 100% alerts.
# Confirm the subscription email AWS sends, or nothing is delivered. Safe to run again.
set -euo pipefail

REGION=${REGION:-ap-south-1}
EIP=${EIP:-3.108.71.220}
EMAIL=${EMAIL:?set EMAIL=you@example.com}
BUDGET_USD=${BUDGET_USD:-15}
export AWS_DEFAULT_REGION=$REGION

acct=$(aws sts get-caller-identity --query Account --output text)
iid=$(aws ec2 describe-instances \
  --filters "Name=ip-address,Values=$EIP" "Name=instance-state-name,Values=running" \
  --query 'Reservations[0].Instances[0].InstanceId' --output text)
[ "$iid" != None ] && [ -n "$iid" ] || { echo "no running instance with $EIP in $REGION"; exit 1; }

topic=$(aws sns create-topic --name lobster-alerts --query TopicArn --output text)
aws sns subscribe --topic-arn "$topic" --protocol email --notification-endpoint "$EMAIL" >/dev/null
echo "topic $topic (confirm the email AWS just sent to $EMAIL)"

# t3.small earns CPU credits; 80% for 15 minutes is not normal for this workload
aws cloudwatch put-metric-alarm --alarm-name lobster-cpu-high \
  --namespace AWS/EC2 --metric-name CPUUtilization --dimensions Name=InstanceId,Value="$iid" \
  --statistic Average --period 300 --evaluation-periods 3 --threshold 80 \
  --comparison-operator GreaterThanOrEqualToThreshold --treat-missing-data notBreaching \
  --alarm-actions "$topic"

aws cloudwatch put-metric-alarm --alarm-name lobster-status-check-failed \
  --namespace AWS/EC2 --metric-name StatusCheckFailed --dimensions Name=InstanceId,Value="$iid" \
  --statistic Maximum --period 60 --evaluation-periods 3 --threshold 1 \
  --comparison-operator GreaterThanOrEqualToThreshold --treat-missing-data notBreaching \
  --alarm-actions "$topic"

# Budgets is a global service; notifications go straight to the email
budget=$(cat <<JSON
{"BudgetName":"lobster-monthly","BudgetType":"COST","TimeUnit":"MONTHLY",
 "BudgetLimit":{"Amount":"$BUDGET_USD","Unit":"USD"}}
JSON
)
notify=$(cat <<JSON
[{"Notification":{"NotificationType":"ACTUAL","ComparisonOperator":"GREATER_THAN","Threshold":80,"ThresholdType":"PERCENTAGE"},
  "Subscribers":[{"SubscriptionType":"EMAIL","Address":"$EMAIL"}]},
 {"Notification":{"NotificationType":"FORECASTED","ComparisonOperator":"GREATER_THAN","Threshold":100,"ThresholdType":"PERCENTAGE"},
  "Subscribers":[{"SubscriptionType":"EMAIL","Address":"$EMAIL"}]}]
JSON
)
if aws budgets describe-budget --account-id "$acct" --budget-name lobster-monthly >/dev/null 2>&1; then
  echo "budget lobster-monthly already exists, leaving it"
else
  aws budgets create-budget --account-id "$acct" --budget "$budget" --notifications-with-subscribers "$notify"
fi
echo "done: cpu alarm, status-check alarm, \$$BUDGET_USD/month budget"