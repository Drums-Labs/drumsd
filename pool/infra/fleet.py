#!/usr/bin/env python3
"""T3 fleet: launch, list and terminate the multi-region pool hosts (AWS), with tags and auto-stop guards.

    python3 fleet.py plan.json up        # coordinator + hosts per region, from the T3 AMI (copied per region)
    python3 fleet.py plan.json status    # every T3 instance in the plan's regions
    python3 fleet.py plan.json down      # terminate every instance tagged Project=drums-sota,Track=T3,Run=<run>

plan.json (example in pool/infra/plan-example.json):
    {"run": "t3-r1", "ami_region": "us-east-2", "ami": "ami-...", "key_name": "drums-sota-t3", "ssh_cidr": "1.2.3.4/32",
     "coordinator": {"region": "us-east-2", "types": ["c8i.8xlarge"], "market": "on-demand"},
     "regions": {"us-east-2": {"hosts": 5}, "us-west-2": {"hosts": 4}},
     "host_types": ["r8i.16xlarge", "r7i.16xlarge", "r6i.16xlarge"], "guard": {"ceiling_min": 240, "idle_min": 30}}

Every instance gets Project=drums-sota, Track=T3, Run=<run>, Role=host|coordinator, a hard power-off ceiling and an idle
rule (pool/infra/guard.sh, installed on the AMI), and InstanceInitiatedShutdownBehavior=terminate. Hosts are spot
(price-capacity-optimized over host_types and every subnet of the region's default VPC).

The AWS calls go through one async function `call(service, operation, region, params)`, so the same code runs with
boto3 (this file's main) or inside any other runner that provides such a call.
"""
import asyncio
import json
import sys

TAGS = {"Project": "drums-sota", "Track": "T3"}


def b64(s: str) -> str:
    import base64
    return base64.b64encode(s.encode()).decode()


def user_data(role, guard, price):
    lines = ["#!/bin/bash", f"bash /opt/drums-t3/guard.sh {guard['ceiling_min']} {guard['idle_min']}"]
    if role == "host":
        lines.append(f"bash /opt/drums-t3/host-firstboot.sh {price} > /var/log/drums-firstboot.log 2>&1")
    return "\n".join(lines) + "\n"


def tags(run, role, name):
    t = dict(TAGS, Run=run, Role=role, Name=name)
    return [{"Key": k, "Value": v} for k, v in t.items()]


async def region_net(call, region, run, ssh_cidr):
    """Default VPC subnets and the run's security group (ssh from ssh_cidr and the coordinator, all traffic inside the SG)."""
    v = await call("ec2", "DescribeVpcs", region, {"Filters": [{"Name": "isDefault", "Values": ["true"]}]})
    vpc = v["Vpcs"][0]["VpcId"]
    s = await call("ec2", "DescribeSubnets", region, {"Filters": [{"Name": "vpc-id", "Values": [vpc]}, {"Name": "default-for-az", "Values": ["true"]}]})
    subnets = [x["SubnetId"] for x in s["Subnets"]]
    name = f"drums-sota-t3-{run}"
    g = await call("ec2", "DescribeSecurityGroups", region, {"Filters": [{"Name": "group-name", "Values": [name]}, {"Name": "vpc-id", "Values": [vpc]}]})
    if g["SecurityGroups"]:
        gid = g["SecurityGroups"][0]["GroupId"]
    else:
        gid = (await call("ec2", "CreateSecurityGroup", region, {"GroupName": name, "Description": f"Drums SOTA T3 {run}", "VpcId": vpc,
               "TagSpecifications": [{"ResourceType": "security-group", "Tags": tags(run, "net", name)}]}))["GroupId"]
        await call("ec2", "AuthorizeSecurityGroupIngress", region, {"GroupId": gid, "IpPermissions": [
            {"IpProtocol": "tcp", "FromPort": 22, "ToPort": 22, "IpRanges": [{"CidrIp": ssh_cidr, "Description": "operator"}]},
            {"IpProtocol": "-1", "UserIdGroupPairs": [{"GroupId": gid, "Description": "pool hosts in this region"}]}]})
    return subnets, gid


async def allow_ssh(call, region, gid, cidr, why):
    try:
        await call("ec2", "AuthorizeSecurityGroupIngress", region, {"GroupId": gid, "IpPermissions": [
            {"IpProtocol": "tcp", "FromPort": 22, "ToPort": 22, "IpRanges": [{"CidrIp": cidr, "Description": why}]}]})
    except Exception as e:  # noqa: BLE001
        if "Duplicate" not in str(e):
            raise


async def launch(call, region, ami, key, gid, subnets, types, n, market, run, role, guard, name_prefix):
    lt_name = f"drums-sota-t3-{run}-{role}-{region}"
    ud = user_data(role, guard, 0)
    data = {"ImageId": ami, "KeyName": key, "SecurityGroupIds": [gid], "UserData": b64(ud), "InstanceInitiatedShutdownBehavior": "terminate",
            "MetadataOptions": {"HttpTokens": "required"},
            "BlockDeviceMappings": [{"DeviceName": "/dev/sda1", "Ebs": {"VolumeSize": 200, "VolumeType": "gp3", "Iops": 6000, "Throughput": 500, "DeleteOnTermination": True}}],
            "TagSpecifications": [{"ResourceType": "instance", "Tags": tags(run, role, name_prefix)}, {"ResourceType": "volume", "Tags": tags(run, role, name_prefix)}]}
    try:
        lt = await call("ec2", "CreateLaunchTemplate", region, {"LaunchTemplateName": lt_name, "LaunchTemplateData": data,
                        "TagSpecifications": [{"ResourceType": "launch-template", "Tags": tags(run, role, lt_name)}]})
        ltid = lt["LaunchTemplate"]["LaunchTemplateId"]
    except Exception as e:  # noqa: BLE001
        if "AlreadyExists" not in str(e):
            raise
        lt = await call("ec2", "DescribeLaunchTemplates", region, {"LaunchTemplateNames": [lt_name]})
        ltid = lt["LaunchTemplates"][0]["LaunchTemplateId"]
    ov = [{"InstanceType": t, "SubnetId": s} for t in types for s in subnets]
    f = await call("ec2", "CreateFleet", region, {"Type": "instant",
        "LaunchTemplateConfigs": [{"LaunchTemplateSpecification": {"LaunchTemplateId": ltid, "Version": "$Latest"}, "Overrides": ov}],
        "TargetCapacitySpecification": {"TotalTargetCapacity": n, "DefaultTargetCapacityType": market},
        "SpotOptions": {"AllocationStrategy": "price-capacity-optimized"}, "OnDemandOptions": {"AllocationStrategy": "lowest-price"},
        "TagSpecifications": [{"ResourceType": "fleet", "Tags": tags(run, role, lt_name)}]})
    ids = [i for x in f.get("Instances", []) for i in x["InstanceIds"]]
    errs = sorted({e.get("ErrorCode") for e in f.get("Errors", [])} - {None})
    return ids, errs


async def describe(call, region, run=None, ids=None):
    flt = [{"Name": "tag:Project", "Values": ["drums-sota"]}, {"Name": "tag:Track", "Values": ["T3"]},
           {"Name": "instance-state-name", "Values": ["pending", "running", "stopping", "stopped", "shutting-down"]}]
    if run:
        flt.append({"Name": "tag:Run", "Values": [run]})
    p = {"Filters": flt}
    if ids:
        p = {"InstanceIds": ids}
    d = await call("ec2", "DescribeInstances", region, p)
    out = []
    for r in d["Reservations"]:
        for i in r["Instances"]:
            t = {x["Key"]: x["Value"] for x in i.get("Tags", [])}
            out.append({"id": i["InstanceId"], "type": i["InstanceType"], "state": i["State"]["Name"], "az": i["Placement"]["AvailabilityZone"],
                        "region": region, "private_ip": i.get("PrivateIpAddress"), "public_ip": i.get("PublicIpAddress"), "role": t.get("Role"),
                        "run": t.get("Run"), "lifecycle": i.get("InstanceLifecycle", "on-demand"), "launch": str(i.get("LaunchTime")),
                        "vcpus": i["CpuOptions"]["CoreCount"] * i["CpuOptions"]["ThreadsPerCore"]})
    return out


async def copy_ami(call, src_region, ami, dst_region, name):
    im = await call("ec2", "DescribeImages", dst_region, {"Owners": ["self"], "Filters": [{"Name": "name", "Values": [name]}]})
    if im["Images"]:
        return im["Images"][0]["ImageId"]
    r = await call("ec2", "CopyImage", dst_region, {"SourceRegion": src_region, "SourceImageId": ami, "Name": name,
                   "TagSpecifications": [{"ResourceType": "image", "Tags": tags("ami", "image", name)}]})
    return r["ImageId"]


async def up(call, plan):
    run, guard = plan["run"], plan["guard"]
    out = {"run": run, "regions": {}}
    regions = dict(plan["regions"])
    cr = plan["coordinator"]["region"]
    regions.setdefault(cr, {"hosts": 0})
    src = await call("ec2", "DescribeImages", plan["ami_region"], {"ImageIds": [plan["ami"]]})
    ami_name = src["Images"][0]["Name"]
    for region, spec in regions.items():
        ami = plan["ami"] if region == plan["ami_region"] else await copy_ami(call, plan["ami_region"], plan["ami"], region, ami_name)
        for _ in range(120):
            s = (await call("ec2", "DescribeImages", region, {"ImageIds": [ami]}))["Images"][0]["State"]
            if s == "available":
                break
            await asyncio.sleep(15)
        subnets, gid = await region_net(call, region, run, plan["ssh_cidr"])
        r = {"ami": ami, "sg": gid, "subnets": subnets}
        if region == cr:
            c = plan["coordinator"]
            r["coordinator"] = await launch(call, region, ami, plan["key_name"], gid, subnets, c["types"], 1, c.get("market", "on-demand"), run, "coordinator", guard, f"t3-{run}-coord")
        if spec["hosts"]:
            r["hosts"] = await launch(call, region, ami, plan["key_name"], gid, subnets, plan["host_types"], spec["hosts"], "spot", run, "host", guard, f"t3-{run}-host")
        out["regions"][region] = r
    return out


async def status(call, plan):
    regions = set(plan["regions"]) | {plan["coordinator"]["region"]}
    rows = []
    for r in sorted(regions):
        rows += await describe(call, r, plan["run"])
    return rows


async def down(call, plan):
    rows = await status(call, plan)
    by = {}
    for x in rows:
        if x["state"] not in ("terminated", "shutting-down"):
            by.setdefault(x["region"], []).append(x["id"])
    for region, ids in by.items():
        await call("ec2", "TerminateInstances", region, {"InstanceIds": ids})
    return by


def boto_call():
    import boto3
    clients = {}

    async def call(service, op, region, params):
        k = (service, region)
        if k not in clients:
            clients[k] = boto3.client(service, region_name=region)
        fn = getattr(clients[k], "".join("_" + c.lower() if c.isupper() else c for c in op).lstrip("_"))
        return await asyncio.to_thread(fn, **params)
    return call


if __name__ == "__main__":
    plan = json.load(open(sys.argv[1]))
    cmd = sys.argv[2]
    call = boto_call()
    fn = {"up": up, "status": status, "down": down}[cmd]
    print(json.dumps(asyncio.run(fn(call, plan)), indent=1, default=str))
