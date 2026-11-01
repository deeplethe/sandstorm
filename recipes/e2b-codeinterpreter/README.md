# `e2b-codeinterpreter`

An E2B template for Sandstorm: a rootfs with E2B's own `envd` in it, so the
official `e2b` SDKs work against a Sandstorm master unchanged.

```python
# E2B_DOMAIN=<your domain>  E2B_API_KEY=<the master's API token>
from e2b import Sandbox

sb = Sandbox.create()                 # template "base"
print(sb.commands.run("id").stdout)   # uid=1000(user) ...
sb.files.write("/home/user/hi.txt", "hello")
sb.kill()
```

## How it fits together

- **envd** is built from `e2b-dev/infra` at a pinned tag (`ENVD_REF` in the
  Dockerfile), not forked. The SDKs switch features on the version envd
  reports, so it has to be the real one; moving to a newer envd is a change
  to `ENVD_REF` and a rebuild. envd is Apache-2.0.
- **The entrypoint** starts `envd -port 49983 -isnotfc -no-cgroups` and then
  the template's own program, if it has one. `tini` is PID 1 so orphaned
  processes are reaped.
- **The master** serves E2B's API and E2B's sandbox addresses when started
  with `--e2b-bind` and `--e2b-domain`. A create gives envd its environment
  and a freshly generated access token through `/init` before answering, so
  the SDK's first request to envd already authenticates.

## Build and register

```bash
# On a machine with Docker:
NAME=base STEPS=build bash recipes/e2b-codeinterpreter/build.sh
# On every node of the pool, with out/base/rootfs.tar copied over:
sudo NAME=base STEPS=unpack bash recipes/e2b-codeinterpreter/build.sh
# Once, against the master:
NAME=base STEPS=register MASTER=https://<master>:8080 PKI_DIR=/var/lib/sandstorm/pki \
  bash recipes/e2b-codeinterpreter/build.sh
```

The code interpreter is the same with E2B's image underneath:

```bash
NAME=code-interpreter-v1 BASE=e2bdev/code-interpreter:latest \
  CMD_JSON='["/root/.jupyter/start-up.sh"]' bash recipes/e2b-codeinterpreter/build.sh
```

The template registers with `ro_binds: []`: the default binds the host's
`/usr`, `/lib` and `/bin` over the template's, which suits a skeleton rootfs
and not a whole distribution.

## Serving the SDKs

```bash
sandstormd serve --master ... --e2b-bind 0.0.0.0:443 --e2b-domain sb.example.com
```

`api.sb.example.com` and `*.sb.example.com` must resolve to that address. The
front door's certificate is issued from the deployment's CA unless
`--e2b-tls-cert`/`--e2b-tls-key` give one (an ACME wildcard, in production);
with the deployment's CA, point the SDK's machine at it with
`SSL_CERT_FILE=/var/lib/sandstorm/pki/ca.pem`.

Without wildcard DNS a sandbox is also reachable as
`https://<front>/sandbox/<id>/<port>/...`, or with the headers
`E2b-Sandbox-Id` and `E2b-Sandbox-Port`, but the SDKs build Host-based
addresses themselves, so the SDKs need the DNS.

To require a token for traffic forwarded to a sandbox, create it with
`network.allowPublicTraffic=false`. The response includes a
`trafficAccessToken`; send it as `e2b-traffic-access-token` on requests to
the sandbox's exposed ports. This applies to the wildcard host, path, and
header routing forms. By default, exposed ports remain public.

## What differs from E2B

- **`sudo` inside a sandbox does not work.** The bundle sets
  `noNewPrivileges`, which a setuid binary cannot get past. envd itself runs
  as root and switches to `user` for each process with `setuid`, which
  `CAP_SETUID`/`CAP_SETGID` allow without loosening anything; commands that
  need root should ask for `user="root"`.
- **The internet is off by default,** which E2B's is not. Turn it on in the
  console, or with `POST /v1/egress`; see [Egress](../../docs/API.md#egress).
- **`secure=false` is unsupported:** it returns HTTP 501 because Sandstorm
  always gives envd an access token.
- **Timeouts:** the master checks expiry every five seconds. An expired
  sandbox is paused when `autoPause=true`, otherwise it is deleted; expiry
  is not exact to the second. Sandstorm's own idle tiers still apply.
- **Not yet acted on:** metrics and logs.
- **Template builds** (`e2b template build`) are not supported. A template
  needs envd in it, so build with this recipe and push the image for
  [`from-image`](../../docs/API.md#post-v1templatesfrom-image), or register with `POST /v1/templates`.
