# Host-side observation: which host uid owns each env's on-disk files. Run on the host with the bare drumsd.
import sys, drums, subprocess, glob, os, json
url, tok = sys.argv[1], sys.argv[2]
c = drums.connect(url=url, token=tok)
a = c.create(); a.wait_valid(60); b = c.create(); b.wait_valid(60)
# have each env write a marker file in its home so there is a fresh file to stat
for e in (a,b):
    e.step({"type":"execute","command":["bash","-lc","echo x > ~/t4owner.txt; cat /proc/self/uid_map"]},check=False)
# find the state dirs on the host and stat an upper file owner
def owner(slotglob):
    outs=[]
    for f in glob.glob(slotglob):
        try: outs.append((f, os.stat(f).st_uid))
        except Exception as ex: outs.append((f,str(ex)))
    return outs
st = subprocess.run(["bash","-lc","sudo ls -d /var/lib/drums/state/slot* 2>/dev/null"],capture_output=True,text=True).stdout.split()
res={"env_a":a.id,"env_b":b.id,"slots":st[:6]}
# the host uid that owns each slot's upper tree
own = subprocess.run(["bash","-lc","for d in /var/lib/drums/state/slot*/upper; do echo -n \"$d \"; sudo stat -c %u $d 2>/dev/null; done"],capture_output=True,text=True).stdout
res["upper_owner_uid"]=own.strip()
print(json.dumps(res,indent=2))
a.close(); b.close(); c.close()
