#!/usr/bin/env python3
"""The official e2b SDK, pointed at Sandstorm with two environment variables.

    E2B_DOMAIN=<--e2b-domain> E2B_API_KEY=<the master's API token> python3 demo.py

With the front door on Sandstorm's own CA, also SSL_CERT_FILE=<pki-dir>/ca.pem.
The full check of every call the SDK makes is scripts/dev/e2b-sdk.py.
"""

from e2b import Sandbox

with Sandbox.create("base", envs={"GREETING": "hello"}) as sb:
    print(sb.commands.run("echo $GREETING from $(id -un)").stdout, end="")
    sb.files.write("/home/user/note.txt", "written through envd")
    print(sb.files.read("/home/user/note.txt"))
    sb.commands.run("for i in 1 2 3; do echo tick $i; sleep 1; done", on_stdout=print)
