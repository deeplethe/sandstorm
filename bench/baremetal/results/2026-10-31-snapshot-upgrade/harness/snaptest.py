#!/usr/bin/env python3
# Snapshot a stateful sandbox, optionally change a bound file, then wake it.
import json,os,shutil,ssl,sys,time,urllib.request,urllib.error
d="/var/lib/sst-snap"; port="18082"; ENV="/opt/sst-snap"
tok=open(d+"/pki/api-token").read().strip(); ctx=ssl.create_default_context(cafile=d+"/pki/ca.pem"); ctx.check_hostname=False
def call(m,p,b=None):
    r=urllib.request.Request(f"https://127.0.0.1:{port}{p}",data=json.dumps(b).encode() if b is not None else None,method=m,headers={"authorization":"Bearer "+tok,"content-type":"application/json"})
    try:
        with urllib.request.urlopen(r,context=ctx,timeout=60) as x:
            raw=x.read(); return x.status,(json.loads(raw) if raw else None)
    except urllib.error.HTTPError as e: return e.code,e.read().decode()[:200]
def get(s,to=8):
    q=urllib.request.Request(f"http://127.0.0.1:{s['gateway_port']}/",headers={"x-sandstorm-sandbox-token":s["token"]})
    t=time.time()
    try:
        with urllib.request.urlopen(q,timeout=to) as r: return r.status,r.read().decode().strip(),round((time.time()-t)*1000)
    except urllib.error.HTTPError as e: return e.code,e.read().decode()[:140],round((time.time()-t)*1000)
    except Exception as e: return 0,str(e)[:90],round((time.time()-t)*1000)
def useold(): shutil.copy(ENV+"/python3.12.old",ENV+"/python3.12.tmp"); os.rename(ENV+"/python3.12.tmp",ENV+"/python3.12")
def upgrade(): shutil.copy(ENV+"/python3.12.new",ENV+"/python3.12.tmp"); os.rename(ENV+"/python3.12.tmp",ENV+"/python3.12")
def up(s,secs=20):
    e=time.time()+secs
    while time.time()<e:
        r=get(s,2)
        if r[0]==200: return r
        time.sleep(0.05)
    return r
def view(i):
    c,v=call("GET","/v1/sandboxes/"+i)
    return {k:(v.get(k)[:70] if isinstance(v.get(k),str) else v.get(k)) for k in ("tier","error","error_attempts") if isinstance(v,dict) and k in v} if c==200 else (c,v)
def demote(i):
    time.sleep(16)
    print(" demote",call("POST",f"/v1/sandboxes/{i}/tier",{"tier":"snapshot"})[0])
    e=time.time()+30
    while time.time()<e and not os.path.exists(f"{d}/snapshots/{i}/checkpoint.img"): time.sleep(0.1)
    time.sleep(3); print(" image on disk:",os.path.exists(f"{d}/snapshots/{i}/checkpoint.img"),view(i))
def timeline(s,i,secs=12):
    t0=time.time(); last=None
    while time.time()-t0<secs:
        r=get(s,3); line=(r[0],r[1].split(" ")[0] if r[0]==200 else r[1][:50])
        if line!=last:
            print(f"  +{(time.time()-t0)*1000:6.0f} ms HTTP {r[0]} took {r[2]} ms: {r[1][:70]}  image={os.path.exists(f'{d}/snapshots/{i}/checkpoint.img')} {view(i)}"); last=line
        time.sleep(0.25)
case=sys.argv[1]
useold()
c,s=call("POST","/v1/sandboxes",{"template":"snapt","port":8000}); i=s["id"]
print(case,"created",c,i[:10])
print(" first",up(s)); print(" second",get(s))
if case=="control":
    demote(i); timeline(s,i,4); print(" last",get(s),"image still there:",os.path.exists(f"{d}/snapshots/{i}/checkpoint.img")); time.sleep(70); get(s); time.sleep(30); print(" 100 s on",get(s),view(i),"image",os.path.exists(f"{d}/snapshots/{i}/checkpoint.img"))
elif case=="snapshot":
    demote(i); upgrade(); print(" upgraded the bound python"); timeline(s,i,20); print(" last",get(s)); print(" view",view(i),"image",os.path.exists(f"{d}/snapshots/{i}/checkpoint.img"))
    useold(); print(" put the old file back"); t=time.time(); r=up(s,150); print(f" back after {time.time()-t:.0f} s:",r,view(i)); time.sleep(70); get(s); time.sleep(35); print(" 105 s on",get(s),view(i),"image",os.path.exists(f"{d}/snapshots/{i}/checkpoint.img"))
elif case=="tag":
    tag=f"snapt-{int(time.time())}"; print(" branch",call("POST",f"/v1/sandboxes/{i}/branch",{"tag":tag})[0]); up(s); upgrade(); print(" upgraded")
    c,k=call("POST","/v1/sandboxes",{"snapshot_tag":tag,"n":1}); k=k[0]; print(" fork",c,k["id"][:10]); timeline(k,k["id"],75); print(" fork view",view(k["id"])); call("DELETE","/v1/sandboxes/"+k["id"])
elif case=="hot":
    upgrade(); print(" upgraded under a hot sandbox"); timeline(s,i,3)
print(" end",view(i),"snapshots:",os.listdir(d+"/snapshots") if os.path.isdir(d+"/snapshots") else [])
call("DELETE","/v1/sandboxes/"+i); time.sleep(2)
