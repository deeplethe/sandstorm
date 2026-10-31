# Changelog

Notable changes per release. forkd follows [Semantic
Versioning](https://semver.org/spec/v2.0.0.html) once it reaches
1.0; until then, the minor version can break compatibility.

## Unreleased

### A template can come from a container image

`POST /v1/templates/from-image`, or "+ From an image" in the console, has every
node pull an image from its registry and build the template its backend reads, with
no Docker on the node. A node that joins later pulls it too. What the image says
to run becomes the template, and `args`, `env` and `port` override it.

Public images only. On Firecracker nodes the image needs `bash` and `python3`.
See [the API](docs/API.md#post-v1templatesfrom-image).

### A sandbox can be let out to the internet

A sandbox could reach nothing beyond its own node, and there was no way to give
one more. Each now has a policy: `internet` for public addresses, `allow` and
`deny` for IPv4 addresses and CIDRs. The console sets it per sandbox and sets the
default for new ones, and `POST /v1/sandboxes` takes `egress`. `internet` never
includes private ranges, other sandboxes or the cloud metadata service.

The default stays closed, so nothing changes until someone opens it. The E2B API
now honours `allow_internet_access: false` and `network.allowOut` and `denyOut`,
and takes `PUT /sandboxes/:id/network`. At creation `true` leaves the decision to
the default, because older SDKs send it on every create. So on an E2B deployment,
turn the default on for code that expects the internet. See
[Egress](docs/API.md#egress).

A sandbox's outbound traffic is now refused rather than dropped where it has no
exception, so a closed sandbox looking a name up fails in about 0.1 s, not 5 s.
A rootfs with no usable resolver gets 1.1.1.1 and 8.8.8.8 in the sandbox's layer
under gVisor, as Firecracker guests already did.

### A port handed on is bound the first time

A sandbox given a port another had just left could fail to bind it, and stay unreachable until the node's next report. A closed listener lives on in any process another thread has just forked, until it reaches `exec`, and a node starts `runsc`, `ip` and `iptables` all the time. The gateway now tries again for up to 100 ms.

### A sandbox waiting to retry costs no polls

The master answers a poll at once if it has a command, and it issues one for every sandbox that has not converged, including one its node is backing off on. So a node that dialled in skipped the command and polled again straight away until the retry was due: about 350 times a second and 40% of a core on bare metal for one failing sandbox, with a log line each time. A node now says what it is putting off and for how long, and the master leaves those out and parks the poll, so 33 s of it went from about 11,500 polls to about 20. Commands for other sandboxes still arrive at once.

### A sandbox that dies on waking keeps its snapshot

A snapshot's image was deleted as soon as `runsc restore` returned, before the program had run. If the sandbox then died, the master recreated it from its template and the caller got a 200 from a sandbox with none of its memory, with no error anywhere. Seen on gVisor with a library replaced under a snapshot, which unattended-upgrades does on its own. The image now stays for a minute after a restore. A sandbox that dies inside that goes back to being a snapshot and fails with a reason, and is tried again at most once a minute, so it comes back with its memory once the files do. A fork of a tag that dies the same way now stops with the same error rather than restarting for ever.

### A warm template survives an upgrade of the host

On gVisor a sandbox runs on the host's own `/usr`, and a warm image holds a program laid out for the files it saw. After unattended-upgrades replaced python3.12 and libc, every sandbox restored from an older image segfaulted 200 ms later, and the node started it again cold, for every create, with nothing to say why. On bare metal that made 100 warm creates take 16 s. The image's key now follows the host files a template binds in, so an upgrade seeds a new one. A sandbox that still dies just after a warm start takes the image with it.

### A node starts sandboxes side by side

A node used to start the sandboxes of a batch one after another, so 20 children of a fork took 45 ms each on Firecracker and 96 ms on gVisor. It now starts one for each CPU at once, and a create promises its memory as it is let through, so a burst is not judged against one reading of free memory. On a 16-thread machine, 20 children went from 1.0 s to 0.33 s on Firecracker and 2.1 s to 0.89 s on gVisor. The namespace pool now follows the machine's memory, 32 to 256, because a burst past the pool waited on the kernel's networking lock: 100 children on Firecracker went from 8.0 s to 1.6 s.

### A repeated destroy leaves other sandboxes' network alone

The master asks for a destroy until a report no longer lists the sandbox, so a repeat can arrive after the first is done. A veth built for that sandbox under its own name had gone back to the pool by then, and the repeat took it down or deleted it. The sandbox holding it could not be reached again. A sandbox whose network does not arrive after it started is now removed and started again, rather than left running where nothing can reach it.

### A Firecracker sandbox woken from disk keeps the right time

A sandbox its node had put to disk to make room still had the rule that refuses connections to a cold sandbox when it came back, and the restore set the guest's clock through that rule. So the wake waited out 2 s, and the clock was not set. The guest is let through as soon as it is running.

### A master can name itself in its certificate

`--tls-san` (`SANDSTORM_TLS_SAN`) adds a name or address to the master's
certificate, and can be repeated. The certificate named only `127.0.0.1`,
`localhost` and the `--bind` address, so a client that dialled a master by any
other name was refused, and one bound to `0.0.0.0` could be verified only from its
own machine. The same goes for a node's `--master-url`: only its registration is
pinned, and every later call verifies the name. A master bound to `0.0.0.0` with
no `--tls-san` now warns.

### A branch refuses every removed `mode`

`mode: "diff"`, or any other value, was ignored, so a caller who asked for a diff
got a full branch and was not told. Everything but `"full"` now answers 410, as
`docs/API.md` said. `GET /ca.pem` is documented.

### The packaged service and image listen where a master does

`packaging/systemd/sandstormd.service` and the `Dockerfile` bound port 8889. Both
now use 8080. The service also no longer passes `--api-token-file
/etc/sandstorm/api-token`, so its token is in `/var/lib/sandstorm/pki/api-token`
like any other master's. A host on that unit gets a new token there on restart
unless the old one is moved.

### The Python `Controller` matches v0.6 too

`branch_sandbox` needs a `tag`. `diff`, `measure_diff`, `mode`, `wait`, `prewarm`,
`live_fork`, `hugepages` and `BranchMode` are gone, and a branch waits as long as
the master may take. `list_templates` and `touch_sandbox` are new.

### The TypeScript SDK and the MCP server reach a master at its default address

Both defaulted to `http://127.0.0.1:8889`, where the v0.5 daemon listened. They
now default to `https://127.0.0.1:8080`, where a master listens, and neither had
a way to trust the private CA a master signs its certificate with. Both now read
`SANDSTORM_CA`, as the Python `Controller` does, and the TypeScript `Controller`
takes `caFile` too. The file replaces the trust store rather than adding to it,
and there is no way to turn verification off.

They could not start anything on a new master either: both needed a snapshot tag,
and a tag only comes from branching a sandbox. Both now start from a template.
`Sandbox.create({ template })` and `ctrl.createSandbox` are new in TypeScript,
`spawn_sandboxes(template=...)` and `list_templates` in the MCP server, and
`Sandbox.create` and `spawn_sandboxes` wait until a node reports the sandbox
running and remove it if it never does. `touch` is new in both, since a branch
needs a hot sandbox.

This breaks callers. A v0.5 daemon, or a master bound to `127.0.0.1:8889`, needs
`SANDSTORM_URL` set, and `caFile` with a custom `fetch` throws, since that `fetch`
would ignore it. The MCP server stops at startup if `SANDSTORM_CA` cannot be read.
What v0.6 refuses or ignores is gone rather than advertised: `diff`,
`measure_diff`, `mode`, `wait`, `liveFork`/`live_fork`, `hugepages`, `prewarm` and
the `BranchMode` type. A branch needs a `tag`. `Sandbox.eval` returns what the
snippet printed, as a string. MCP `spawn_sandboxes` no longer sends
`memory_limit_mib=256`, which overrode every template's memory, and takes
`template` as its first argument. The TypeScript types describe the master's
responses, so fields such as `pid` and `pause_ms` are gone.

Exec, eval and branch now wait as long as the master may take rather than timing
out first at 60 s.

### The Python `Sandbox` talks to the master

`sandstorm.Sandbox` drove the `sandstorm` CLI, which left the tree in v0.6, so
it could not start anything. It now creates through the master's API, from a
template (`Sandbox("python")`) or a tag (`Sandbox(tag="checkpoint-1")`), waits
for a node to report the sandbox running, and sends its token on exec, eval
and ping. `Sandbox.connect(id, token)` wraps one that exists, such as a child
of `Controller.spawn_sandboxes`, and `Controller.create_sandbox` is new.

This breaks callers. `target=`, `spawn=False` and `SANDSTORM_TARGET` are gone,
`Sandbox()` no longer defaults to the tag `pyagent` and needs a template or a
tag, a master's refusal raises `ControllerError` where `commands.run` used to
return it, and `eval` returns what the snippet printed, as a string. `Controller()`
with no address now goes to `https://127.0.0.1:8080`, where a master listens,
not `http://127.0.0.1:8889`.

### What the v0.5 CLI left behind is gone

The recipes that needed the `sandstorm` CLI, the Hub and its registry, the Arch
package, the Kubernetes manifest, the v0.5 host scripts and the docs that
described them are removed. Most are in the `v0.5.3` tag, and all are in git
history. `recipes/e2b-codeinterpreter` stays. `docs/API.md` is rewritten from the
routes that exist. The MCP server's `create_snapshot` tool is removed: it called
`POST /v1/snapshots`, which no longer exists.

### A node's backend is chosen from the console

"Add a node" offers Auto, gVisor or Firecracker and carries the choice into the
install command (`SANDSTORM_BACKEND`, and `SANDSTORM_FC_KERNEL` and
`SANDSTORM_STATE_DIR` where they are needed). The installer checks the machine
before it installs anything, then has the node's own binary check again
(`sandstormd preflight`, the checks a node makes when it starts), and refuses
saying what is missing. It now also refuses gVisor where runsc is not
installed, which used to be a warning. An empty node can change backend later:
`POST /v1/nodes/:id/backend` has the node rewrite `--backend-file` and restart.
Refused while the node holds any sandbox or snapshot. Nodes installed before
this have no backend file and have to be reinstalled to switch.

Nodes now report the templates they can start on their backend, the master
places a template only on those, and a create can ask for a `backend`. A
create no node can run is a 409 rather than a failure on a node.

### A sandbox that fails to start on gVisor is retried

When `runsc start` failed, for example on a template whose program is not in
its rootfs, the container was left stopped with its sandbox process alive. The
node reported that as a running sandbox, so the master cleared the failure and
never retried, and commands sent to it failed or timed out. The node now removes
the container, and such a sandbox is retried and then quarantined with
`last_error` set, as other failures to start are. A failed restore is cleaned up
the same way.

### A checkpoint that fails leaves no snapshot behind

A checkpoint used to create `snapshots/<id>` before it ran and leave it there
if it failed or timed out, and the node reported any such directory as the
Snapshot tier: the master stopped retrying, and a wake restored from nothing.
Each attempt now writes into a directory of its own, the backend checks the
image's files (`Backend::validate`), the image is sealed with the length of
every file, and only then is it renamed into place without replacing
anything. A tag from `branch` is published the same way. A sandbox with
processes in its cgroup is reported by them, whatever is on disk.
### The official E2B SDKs work against Sandstorm

`sandstormd serve --master --e2b-bind 0.0.0.0:443 --e2b-domain <domain>`
serves E2B's REST API at `api.<domain>` (create, get, list, kill, connect,
resume, pause, timeout, refresh, templates; `X-API-Key` is the master's API
token) and every sandbox at `<port>-<id>.<domain>`, so `e2b`'s Python and JS
SDKs work with only `E2B_DOMAIN` and `E2B_API_KEY` set. Sandboxes need a
template with E2B's `envd` in it: `recipes/e2b-codeinterpreter` builds one.
A create answers once envd has taken its environment and a freshly generated
access token. `hot` and `frozen` read as `running`, `snapshot` and `detached`
as `paused`. On bm the SDK's create, connect, commands (streamed), PTY,
files, `watch_dir`, `get_host`, pause and resume all pass
(`scripts/dev/e2b-sdk.py`), on gVisor and Firecracker, with `e2b` 2.50 and
2.62: the SDKs moved create and connect to `/v2` in 2.51, and both are served.

Not yet: the `timeout` deadline is recorded but nothing acts on it, and
`secure=false`, `allowInternetAccess=false`, metrics and logs are accepted
and ignored. `sudo` inside a sandbox is refused by `noNewPrivileges`.

### The gateway streams, and one host port reaches every port of a sandbox

The node gateway used to read a request's whole body, send it, then read the
sandbox's whole answer before replying, with a 60 s limit on all of it. A
server stream -- envd's `commands.run`, server-sent events, a long download
-- arrived in one piece at the end or not at all. It now speaks HTTP/1.1 on
a connection of its own per request and passes both bodies through as they
arrive, with no overall time limit; a WebSocket upgrade is spliced through.
The sandbox counts as in flight until its answer has been read to the end,
not only until its headers arrived.

`x-sandstorm-port: <port>` sends a request to that port inside the sandbox
instead of the one it was created with. The token check is the same for every
port, and neither header reaches the sandbox.

### Design decisions are ADRs

The design documents at the root are gone: `DESIGN.md`, the five
`DESIGN-v0.4*.md` files, `DESIGN-v0.5-diff-snapshot-chains.md`,
`DESIGN-v0.6.md`, and `docs/design/`. In their place,
[`docs/adr/`](docs/adr/README.md) holds one architecture decision record per
decision v0.6 is built on: gVisor rather than KVM, the four tiers, the master
and its nodes, mTLS, pools, waking on traffic, nodes that protect themselves,
making room on a wake, one binary, and the optional Firecracker backend. Each says only what holds for the code.

`DESIGN-v0.6.md` was the RFC v0.6 started from, and parts of it were changed
or dropped as it was built -- the `cold` tier, PostgreSQL on the master, gRPC
between master and node. Its last version is at
[`7b39861`](https://github.com/deeplethe/sandstorm/blob/7b3986150b71feeece318ce1b4475f3c291d3aae/DESIGN-v0.6.md);
the Firecracker-era notes are in the
[`v0.5.3` tag](https://github.com/deeplethe/sandstorm/tree/v0.5.3), and
relative links to them further down this file now point there.

### The v0.5 Firecracker runtime leaves the tree

`sandstorm-cli`, `sandstorm-vmm`, `sandstorm-uffd` and the `experiments/`
crates are removed, with the design notes, benchmarks and scripts that only
described or drove them; the guest-kernel scripts stay, since
`--backend firecracker` boots that kernel. `sandstormd` never depended on any of them; they
were still workspace members, so CI built them and readers took them for
part of v0.6. Everything removed is in the
[`v0.5.3` tag](https://github.com/deeplethe/sandstorm/tree/v0.5.3), and
links that pointed at it now point there. `docs/RUNBOOK.md` and
`docs/ROADMAP.md` described only the v0.5 runtime and are now short
pointers. The `/v1/sandboxes` refusal of `live_fork` and `hugepages` stays.

### forkd is now Sandstorm

Everything is renamed at once, with no compatibility layer: the crates
(`sandstorm-core`, `sandstorm-cli`, ...), the daemon (`forkd-serve` is now
`sandstormd`), the CLI (`sandstorm`), environment variables (`SANDSTORM_*`),
paths (`/var/lib/sandstorm`, `/etc/sandstorm`), systemd units, HTTP headers
(`x-sandstorm-*`), metrics (`sandstorm_*`), the iptables chain
(`SANDSTORM-COLD`) and the veth prefix (`sst`). The SDKs are
`@deeplethe/sandstorm` on npm, `sandstorm-sdk` on PyPI (imported as
`sandstorm`) and `sandstorm-mcp`. Entries below this one describe forkd and
keep its names.

### One console, at `/`

The console is now the only web page: `/` serves it as well as `/console`,
and the older panel is gone. It gains **Add a node**, with both ways a node
joins -- a single-use command for a machine that dials in, and an address and
pairing code for one the master connects to -- the README's mark and
wordmark in its header, drawn as SVG in the theme's colours, with titles and
figures set in the wordmark's Newsreader (a 22 KB subset inlined, since a
master may have no internet), one neutral accent so colour is left to the
data, plain tabs over a sliding underline, cards that are outlines with no
fill on every tab, on columns that meet at the same edges, three corner radii
(card, control, tag) used everywhere, a favicon in place of forkd's bolt, and
sparklines that draw in once instead of replaying the sweep on every refresh.
The header says the time of the last update rather than a count of seconds
that ticked every second, and has a button to refresh now.

What the panel had and more is in the console too: every sandbox, searchable
and filtered by tier, node or failing, a page at a time, with wake, freeze,
to-disk, retry and delete on each; **New sandbox**, which shows the token
the one time the master hands it out; and the snapshots, with delete.
`GET /v1/sandboxes` takes `tier`, `node`, `q`, `failing`, `limit` and
`offset` for this, and says the matching total in `x-total-count`; with none
of them it is the whole list, as before.

It is in five tabs -- Overview, Nodes, Sandboxes, Snapshots and Activity --
kept in the URL's hash, with counts on the tabs and a highlight that slides
to the one picked. The sandbox and snapshot lists, and the activity, are
fetched only while their tab is open.

The overview fits one screen wherever the window is at least 1201 by 620:
the cards share the height under the header, and the charts draw to the box
they get. Smaller, it scrolls as before. Its tier flow is a map: the four
tiers in a square, each with its count, share of the fleet, how long idle
puts a sandbox there and its count over the range; idle carries sandboxes
clockwise round it -- freeze, to disk, detach -- and a request wakes one
straight back to awake from wherever it is, a line from each tier, every
line written with its measured rate. For this the master now counts wakes by
the tier they come from: `rate.wakes` has a series for each of frozen,
snapshot and detached beside the total. It takes the
left half of the overview; beside it are sandboxes by tier, lifecycle rates,
latency, and a new **Node headroom**: free memory on each node now, tightest
first, against the floor. It replaces an animation of random particles,
whose tanks filled relative to the largest tier, and a donut that showed the
same four counts again. The KPI sparklines are a quiet grey but for Awake, in its
tier's colour, and for a figure gone wrong -- a silent node, refused
creates, an API p99 over 250 ms -- in the colour that says so; hovering a
KPI, a tier, a move or a node says what it is, like the charts.

Every choice list is the console's own dropdown rather than the system's
menu, focus is shown in the theme's ink rather than the browser's blue, a
dialog's two ways are a segmented control with its action a button its own
size, the snapshot builder's steps are outlines like every other box, and a
node's memory pressure is one thin bar across its column, as the node cards
had it, with the figure in its hover.

A count on the overview that moved since the last refresh rolls to its new
value in green going up or red going down, and the change pops in beside it,
+N or −N, gone again in under two seconds. Table and filter rows read calmer:
a node's row is centred top to bottom, a page's filters are one segmented
control with the failing count in red text rather than a pill. The header has a
sun or moon for the theme and a link to the repository.

Until the master first answers, the page under the header is a loading
screen, the logo's grains swirling, rather than a page of zeros; it says so
if the master cannot be reached, and comes back if a new time range is slow
to arrive. An answer for a range already left is dropped rather than drawn.
Activity's icons are line drawings like the header's, not emoji.
The tab's icon is the logo's swirl, its grains grown so it reads at 16 px,
in the lighter orange on a dark system; the old one never loaded, its quotes
ending the attribute early.

Sandboxes that will not start are a count on the sandbox table's Failing
filter, which lists them with their errors -- not a line above the table
nor a second list of the same rows under it. The Nodes tab shows its nodes as a table --
free memory, sandboxes by tier, pressure, features, when last seen -- or as
their memory over time, one view at a time on the same search, filter and
page. Over time is a strip a node on one scale and one time axis, with
a legend in MiB, so a column read down is one moment across the fleet; time
under the master's floor is red. The floor and the pressure ceiling the
table and strips mark are the master's own, now in `GET /v1/console/now`
as `floor_mib` and `max_psi`.

Sandboxes, nodes and activity share one pager: the range shown, the pages
within reach, and 20, 50 or 100 a page, 20 to start with, remembered per
table. A sandbox is one line: id, template, node, tier with any error cut to
fit beside it, address and last activity. Nodes can be
searched and narrowed to answering, silent or under pressure. Activity is a
table like the sandboxes', narrowed by kind, a search and how far back;
`GET /v1/console/events` takes `offset`, `kind` (comma-separated) and `q`,
and says the matching total in `x-total-count`.

**Build a snapshot** on the Snapshots tab: start a sandbox from a template or
a snapshot, run commands in it one after another with their output, and
save it under a name, optionally deleting the build sandbox and forking one
from the result. Every step is a public API call, and a build survives a
reload of the tab.

A machine that ran a forkd node keeps forkd's iptables rules until they are
removed: an `OUTPUT -j FORKD-COLD` jump and a chain that rejects TCP to the
sandbox addresses forkd had parked. Sandstorm hands out addresses from the
same 10.86.0.0/16, so a new sandbox that lands on one of them cannot be
reached and its gateway answers 502. Take the old node down with forkd's own
`scripts/fleet.sh down`, or remove the rules by hand:

    sudo iptables -D OUTPUT -j FORKD-COLD
    sudo iptables -F FORKD-COLD && sudo iptables -X FORKD-COLD

### A create a node holds back is no longer given up on

A node short of memory holds a create back and retries it, and reported each
hold-back as a failure. Five of them and the master quarantined the sandbox:
it stopped asking, and nothing started it until someone called `/retry`. At
20 creates a second, 1,246 of 20,000 ended that way, on nodes that had room
for them minutes later. A hold-back is now reported as one (`held_back`), kept
as the reason the sandbox is not up yet, never counted towards quarantine,
and looked at again within 5 s rather than backing off towards a minute.

And a sandbox's error now clears as soon as a report shows it up with nothing
failing. It used to stay until `/retry`, so sandboxes that were running fine
still read as failed.

### A sandbox that dies is cleared away, and its slot is not shared

A sandbox whose processes died -- killed by the OOM killer, say -- left a
bundle with nothing running in it. The node does not report one of those,
so the master never asked for it to be destroyed: its network namespace and
veth stayed, while the master handed its slot to a new sandbox. Both host
ends then held the same address, the route went to the dead one, and the
new sandbox, running, could not be reached. The node now clears away a
sandbox that has had nothing running for a minute, network included -- one
the master still wants is then created again, so a crash restarts it -- and
setting up a slot removes any other veth still holding its address.

### A wake makes room instead of refusing

A request that reached a sandbox on disk while its node was under its
memory floor was answered 503. With 40 sandboxes per node where about 12 fit
awake, that was 13% of requests at 5 req/s and a third at 40, while frozen
sandboxes nobody had called for minutes held the memory.

A wake -- from the gateway or from the master -- now puts the sandboxes
least in demand on the node to disk first, frozen ones before hot ones,
until the node is back over its floor. A sandbox with a request in flight
is never chosen, nor one that came up in the last 2 s with no request yet,
nor one whose tier is already changing. At most four
go per wake; past that, or with nothing left to choose, the wake is refused
as before. Memory pressure (PSI) is not made room for: it is a 10-second
average that a checkpoint does not move. A wake from the gateway waits for it
instead, for what is left of its budget (below), holding its restore permit
so the wakes behind it queue rather than add to it; with four requests at
once to a full node's snapshots, 8 of 600 had been refused for a few seconds
of pressure their own restores caused. `--wake-max-psi` sets the ceiling a
wake is held to, separately from creates.

The restore permit belongs to the wake, not to the request that started it.
It used to go with the request: a client that gave up while its wake waited
out pressure handed the permit on, the wake carried on regardless, and the
next one did the same. With 80 sandboxes per node at 8 req/s and a client
timeout of 60 s, 24 restores started in the same second on a 2-vCPU node
once the pressure eased; together they ran it out of memory and swap, and it
stopped answering for an hour and a half until the kernel's OOM killer
caught up. A wake now holds its permit until it ends, and one whose caller
gives up while still queued never starts. Reproduced on bm with 12
sandboxes: 12 restores at once before, 2 after.

What spares a sandbox from being put away is a request in flight to it,
not a recent one. Sparing whatever had been used in the last 2 s, counted
from when the request arrived, failed both ways at 80 per node and 8 req/s.
A request waited 24 s (p50) for its wake, so the sandbox came up looking
long idle and the next wake put it to disk 0.24 s later: 98% of the 502s,
7% of all requests, and the same stale time let the master's idle demotion
through its grace period. Counted from the answer instead, nearly every hot
sandbox was spared at once, and 26% of requests were refused for want of
anything to put away. Which sandbox goes is now decided by demand -- each
request counts one, decaying by `e` every 30 s -- so one called once goes
before one called often, and of two called once, the one called longer
ago. A sandbox woken for a request counts as used from then, for the
master's demotions as well; a restore the master asks for is spared for
2 s, like a create, for its request to arrive; and a wake making room
checks again, once it holds a sandbox's lock, that nothing has reached it.
Every 502 the gateway answers is logged with the reason.

A wake that cannot start quickly is refused rather than queued. It has 2 s
(`--wake-budget-ms`) to get a restore slot and for memory pressure to ease,
and is answered 503 with `retry-after` once that is gone; one that would be
behind more than twice as many waiting wakes as there are slots
(`--wake-max-waiting`) is refused at once. Unbounded, a popular sandbox's
wake waited 12 s behind wakes of sandboxes called once, and the answered
requests' p50 went from 13 ms to 4.6 s at 80 per node and 8 req/s.

The node lists what it put away in its reports (`evicted`), and the master
moves desired state there rather than asking for the sandbox back -- which
would make the node evict something else to make room for it. The panel's
counters show the total as `evicted`.

### A web panel, and a join command that works

`GET /` serves an admin panel: nodes with their pool, fingerprint and
pressure; sandboxes with their tier and gateway port, with wake, freeze and
delete; snapshots; and **Add a machine**, which mints a single-use token and
shows the one line to run on it.

One HTML file built into the binary. No build step and nothing to deploy
separately, so the panel is always the version of the master it talks to and
a single-box install stays a single file. The API token is typed in and kept
in that tab's `sessionStorage` -- never written to disk by the page, gone
when the tab closes.

**`POST /v1/node-bootstrap` was handing out a command that could not work.**
It produced `curl -sSfL <master>/install.sh | …`, and nothing served
`/install.sh`: the string appeared once in `api.rs` and the router had no
such route. The endpoint existed, its output 404'd. Now there is a real
script, and `/ca.pem` and `/forkd-serve` beside it.

A joining node fetches **the master's own binary**, so the two cannot drift,
and joining needs no release infrastructure and no internet -- which matters,
because the machines forkd is developed on have neither.

On trust, because the command runs as root: the registration token carries
the CA's SHA-256, and the script fetches the CA, refuses it unless the
fingerprint matches, and does everything after with `--cacert`. The first
fetch of the script itself cannot be verified that way -- the fingerprint is
inside its own argument -- so that one hop uses `-k`. **The API response says
so (`first_fetch_is_unverified`) and the panel says so where it shows the
command**, with the alternative: copy `pki/ca.pem` over and set `FORKD_CA`.

### CodeQL

- **`js/polynomial-redos` (high)** in the TypeScript SDK. Stripping trailing
  slashes with `/\/+$/` backtracks on a long run of slashes, O(n²). The
  input is the caller's own base URL rather than anything hostile, so the
  real risk was small, but this is a published library; it is a loop now.
- **`actions/missing-workflow-permissions`** on every job in `ci.yml` and
  `ci-typescript.yml`. Both only check out and test, so they now declare
  `contents: read` instead of taking the default token's scope. The publish
  workflows already declared theirs per job, which is why they were not
  flagged.
- **`py/bind-socket-all-network-interfaces`** is dismissed with a reason, not
  fixed. The agent is PID 1 inside the sandbox's own netns, where `0.0.0.0`
  covers that namespace alone; `127.0.0.1` would make it unreachable from
  the node over the veth. What confines it is `node/net.rs` dropping sibling
  and off-box traffic and `node/gateway.rs` requiring the sandbox's token.

`scripts/dev/panel.sh` is the check, 19/19 on bm -- including running the
minted one-liner for real: the CA check fires, the binary installs, the unit
starts, the token drop-in is 0600, and the node joins.


### The sandbox gateway: the access token now guards something

RFC 16.5 gave every sandbox a token and said it is required on any of its
ports. There were no ports: a sandbox listened inside its own namespace and
the node's rules dropped siblings and off-box traffic, so it was reachable
only from its own node and the token had nothing to gate. **`gateway_port`
on the create response is where to reach it, and that port requires the
token in `x-forkd-sandbox-token`.**

**Why the gateway runs on the node.** Three parts of the RFC pull apart and
only this placement satisfies all three: §8 wants exposure to be a plain L4
DNAT, §16.5 wants the token checked -- which L4 cannot do, since a DNAT has
nowhere to read a credential -- and §15.5 forbids the data path from
crossing the master. So it cannot be a DNAT and cannot be on the master.

**What that costs:** the exposed port is HTTP in v0.6. A raw TCP stream has
nowhere to carry a token, so a protocol-agnostic gateway would be a DNAT
again. Exposing a non-HTTP port needs a different credential mechanism and
is not built.

- A cold sandbox's exposed port **refuses**, as §8 specifies: the node
  withdraws the listener when the sandbox stops serving. Measured at 9 ms,
  the same answer #327 gives on the sandbox's own port.
- The credential is **not forwarded** to the sandbox, which should never see
  a secret it could log.
- The port and token live in the bundle at 0600, so a node restart
  re-exposes the sandbox rather than reopening a port with nothing to check.

This revives `allocate_port`, added in #324 when host ports had to be unique
per node, made redundant by per-sandbox namespaces in #328, and unused since
-- called only by its own tests while `--port-min`/`--port-max` stayed on the
CLI. The gateway needs exactly what it does.

### Fixed: `serve --master --node` could not start over TLS

The single-command deployment has been broken since TLS became the default.
With `--master` set, the node minted its own registration token straight
from the store, skipping the API -- and so skipping the CA fingerprint the
token carries. It then had nothing to verify the master with and its own
loopback handshake failed with `invalid peer certificate: UnknownIssuer`.

Both earlier mTLS checks ran the two halves as separate processes with
tokens minted through the API, so neither covered the one-process case.
`scripts/dev/gateway.sh` uses it, which is how this surfaced.


### Knowing a sandbox id is no longer enough to enter it

RFC 16.5 layer two. Every sandbox has always been created with a 128-bit
token, returned to the caller -- and **nothing verified it**. The API's
bearer token was the only credential, so holding it meant holding every
sandbox on every node.

`exec`, `eval` and `ping` now require the sandbox's own token in the
`x-forkd-sandbox-token` header, compared in constant time. Mandatory, with
no way to turn it off: v0.5's traffic token was opt-in, which left every
sandbox made before anyone enabled it open, and the RFC removes that mode
rather than defaulting it on.

**Breaking for SDK callers of `exec`/`eval`/`ping`.** Both SDKs take the
token now -- `sandbox_token=` in Python, `sandboxToken` in TypeScript -- and
it is the `token` field of the response that created the sandbox.

**Sandbox ids are 128 bits**, not the 48 that `sb-` plus 12 hex characters
gave. An id appears in URLs and is handed to callers; 48 bits is inside
reach of someone who can make requests, which is what the RFC means by "no
longer a guessable counter".

Still missing, and stated because the token reads as covering it: there is
no gateway in front of a sandbox's own port. One is reachable only from its
own node, so this token guards the API path and nothing else yet. When port
exposure lands it has to check the same token.

### A pool is a migration domain, not a label

**Verified across three machines over the public internet**, which is where
the point of it shows: `xm-bm` (AMD EPYC, kernel 7.0) and `xm-bj` (Intel
Xeon, kernel 5.15) both asked for the pool `default`, produced different
fingerprints, and the mismatched one was moved to `default#0871d6ec` on its
own. Every sandbox asked for in `default` then went to the one node still in
it. On a single machine this is untestable -- two nodes there share a
fingerprint.

Both nodes dialled out from behind NAT to a master on a third machine, with
no tunnel and no firewall change: raw TCP, mTLS end to end, each node
arriving from its own NAT egress address.


The host fingerprint was computed by every node at registration and used
for **nothing**. A node with a different runsc version or CPU could join a
pool, and the mismatch surfaced only when a restore failed on it -- the
"discover it on failure" behaviour RFC section 15 set out to avoid.

- A node whose fingerprint does not match the pool's **joins a derived pool**
  (`<pool>#<fp>`) instead, and the master logs why. An upgraded node leaves
  its old pool on its own rather than quietly poisoning it.
- `snapshots.fingerprint` records the host a tag was baked under. A fork
  directed at a node that cannot restore it is refused with 409 at schedule
  time instead of failing on the node.

### veth names no longer assume one node per machine

Found by running two nodes under one master for the first time: both ends of
a veth pair are created in the **host's** namespace before one is moved into
the sandbox's, and the names were `fkd<index>h`/`fkd<index>s` from the
network index -- which the master allocates per node. Two nodes both started
at 0 and collided, `Cannot find device "fkd0s"`, which retry and backoff hid.
Names are keyed on the sandbox now.

`scripts/dev/cross-machine.sh` is the three-machine check, 7/7, and
`scripts/dev/two-nodes.sh` is the single-box one, 10/10 on bm: both nodes connect over
mTLS, six sandboxes split 3/3 by least-loaded placement, a 128-bit id and
token, exec refused without the token and with another sandbox's, exec
working with its own, and ping demanding it too. After the naming fix the
node logs carry no warnings at all, which is how the collision was confirmed
gone.


### The TypeScript SDK requires Node 20

**Breaking for consumers on Node 18.** `engines` is now `>=20` and CI tests
on 20 and 22. The SDK's own code is unchanged and still only needs the
global `fetch`, which Node 18 had; the floor moved because its test runner
could not be patched without it.

All 19 open Dependabot alerts are cleared, `npm audit` reports none. Every
one was in the dev toolchain or a recipe -- the SDK publishes no runtime
dependencies -- but two were critical (`tinypool` prototype pollution to
RCE, and the Vitest UI server's arbitrary file read), and the chain to fix
them ran: vitest 4.1.12 needs Node >= 20, and npm 11 is needed to install
vitest 4 at all, and npm 11 itself needs Node >= 20.17.

| | from | to |
| --- | --- | --- |
| vitest | 2.1.9 | 4.1.12 |
| vite | 5.4.21 | 8.3.5 |
| postcss | 8.5.15 | 8.5.31 |
| source-map-js | 1.2.1 | 1.2.2 |
| esbuild, tinypool | 0.21.5, 1.1.1 | gone from the tree |
| requests (recipe) | 2.32.5 | 2.33.0 |

Two pieces of published metadata were wrong, so both were measured rather
than trusted:

- Dependabot gives 4.1.11 as the first patched vitest. `npm audit` against
  a tree at 4.1.11 still reports both advisories, with the range
  `<=4.1.11`. 4.1.12 is clean.
- npm 10.9.9 cannot install any vitest 4.1.z: Arborist crashes with
  `Cannot read properties of null (reading 'edgesOut')`, reproducible in an
  empty directory with no lockfile. 4.0.0 and 3.2.7 install; npm 11 installs
  4.1.12. Both TypeScript workflows now install npm 11 first.

lint, build and all 14 SDK tests pass on vitest 4.1.12 with no test changes.

### Security documentation now says which boundary you get

`docs/SECURITY.md` claimed every child ran in a KVM-backed microVM, so
escaping needed a KVM or Firecracker vulnerability -- "the same boundary
AWS Lambda relies on". `forkd-serve` runs gVisor, which shares the host
kernel, so that was not true of any v0.6 deployment. The threat model now
states the boundary per backend, and says why gVisor is the default:
Firecracker needs `/dev/kvm` and most cloud instances do not expose nested
virtualisation, so it cannot be the default for the machines operators
actually have. Firecracker stays supported, and stronger, on hosts with KVM.

The same pass corrected the posture table, which still described v0.5:
`--token-file`, `--tls-cert`, TLS off by default, a shared host bridge.
Added a **Known gaps** section for things that read as covered and are not
-- chiefly that every sandbox is issued a 128-bit token which nothing
verifies, because there is no gateway in front of a sandbox for it to gate.

`is_safe` for snapshot tags now lives in one module (`tag.rs`) and is used
by `create_sandbox`, `branch_sandbox` and the node. `create_sandbox` had no
check at all: the node refused a bad tag before touching the filesystem, so
traversal was blocked, but the tag reached the database first and failed
later with a message about a missing directory. Validating in some handlers
and not the one that joins a path is the exact shape of the 0.1.4 advisory,
so it is a test now.

The *Past advisories* section is removed; the 0.1.4 fix it described is in
this file under its release, with the affected versions.

### The control plane runs on mTLS

`identity.rs` has issued a CA, a master server certificate and per-node
client certificates since the first v0.6 commit, and tested both sides of
the handshake. Nothing served TLS, so every certificate was generated and
then ignored while registration tokens, the API token and every command run
inside a sandbox crossed the network in the clear. `forkd-serve` now serves
TLS by default and verifies both directions (RFC 16.5).

- **The node is identified by its certificate, not by its message.** The
  bearer token is one shared secret, so a token holder could poll as any
  node and be handed its sandboxes. The master records the SHA-256 of the
  certificate it issues (`nodes.cert_sha256`) and resolves each connection
  through it; a node presenting a valid certificate and claiming another
  node's id gets 403.
- **First contact is verified too.** A registering node has no CA yet, so a
  registration token now carries the CA fingerprint it should pin --
  `<fingerprint>.<secret>`, as k3s puts the CA hash in `K3S_TOKEN`. A master
  that does not hold the pinned CA fails the handshake, so the token is
  never sent to it. Tokens minted before this keep working, and the node
  warns that it cannot verify the master.
- **`POST /v1/node/revoke/:id`** stops a node: the master refuses its
  certificate and its parked poll is woken so the refusal lands at once.
- **`--insecure-transport`** restores plain HTTP for a single box on
  loopback. It also turns off the client-certificate check, because there is
  then no certificate to check, and logs what it gives up.

A node that gets a refusal from the master now reports it. Previously the
poll response was deserialised without checking the status, so every
refusal became `error decoding response body` and an endless backoff -- on
bm a revoked node retried forever without ever saying it had been revoked.

`scripts/dev/mtls.sh` is the verification, 12/12 on bm as two processes:
plain HTTP gets nowhere against the port, an untrusting client refuses the
master, a certless caller holding a valid bearer token is refused, a node
registers through the pinned token and connects, and revoking it stops it.


### forkd-controller is removed; forkd-serve replaces it

The v0.5 daemon is gone. `forkd-serve` runs the master, a node, or both
in one process and covers everything the controller served:

| v0.5                            | v0.6                              |
| ------------------------------- | --------------------------------- |
| `forkd-controller serve`        | `forkd-serve serve --master --node` |
| `--state state.json`            | `--state-db state.db` (sqlite)    |
| `--token-file`                  | `--api-token-file`, generated on first run |

The `/v1/sandboxes`, `/v1/snapshots`, `exec`, `eval`, `ping` and `branch`
request and response shapes are unchanged, so both SDKs work against it
without modification -- the Python SDK was driven against forkd-serve
end to end to confirm it rather than assumed.

Options that cannot be honoured return **410 Gone** naming what replaces
them, rather than being ignored: `mode: "live"`, `wait`, `live_fork`,
`hugepages`, `diff`/`measure_diff` (they measured Firecracker diff
snapshots, and gVisor has none), and `per_child_netns: false`, which now
asks for weaker isolation than every sandbox gets.

**Breaking.** `forkd-controller` no longer ships, the systemd unit is
`forkd-serve.service`, and the container image is `forkd-serve`. The
Firecracker crates (`forkd-vmm`, `forkd-uffd`) remain and are unused by
`forkd-serve`, which runs sandboxes on gVisor.

### Reflink copies actually reflink

`chain::reflink_copy` passed `0x40209409` as `FICLONE`; the real number is
`_IOW(0x94, 9, int)` = `0x40049409`. The kernel answered the wrong value with
`ENOTTY`, which the helper treats as "filesystem has no reflink", so every
chain-assembly base copy and the bake's rootfs baseline clone streamed a full
copy even on btrfs, XFS, and ZFS 2.2+. Found by @jrimmer in #321.

### Upgrade note: legacy sandbox rows block startup

The controller now persists a boot identity (start time + boot id) for every
sandbox it registers and, on startup, refuses to kill an orphaned Firecracker
process whose identity it cannot verify — including rows written by an older
forkd, which carry no boot identity. If you upgrade while sandboxes are live,
the controller will abort startup with a `legacy registry row` diagnostic
instead of risking a kill against an unverified PID. Kill the recorded PIDs
manually (or remove the entries from `state.json` after confirming the
processes are dead) before restarting; every sandbox spawned by the new
version persists full identity, so this recurs only for pre-upgrade rows.
Spawns are now also rejected up front if the boot id cannot be read, rather
than registering a sandbox that could never be verified later.

### Safe re-snapshot of existing tags

Re-running `forkd snapshot` on an existing tag no longer risks destroying
the last usable snapshot (#296 follow-up, review #295). The previously
published rootfs is renamed aside (`<tag>.rootfs.ext4.prev-<pid>`) before
the fresh baseline is cloned and booted; any failure — boot timeout,
snapshot error, publish error, interrupt — restores it automatically and
drops the partial clone. The backup is deleted only after the new
snapshot is fully published, so a tag always holds a consistent
(rootfs, vmstate, memory.bin, snapshot.json) set. Also fixes a
src == dst guard comment duplication and makes the chain-unpack tail
bail instead of resolving an empty path.

A later `snapshot` run on that tag now sweeps every
`<tag>.rootfs.ext4.prev-*` file instead of only the one named for its own
pid. The backup name is pid-scoped, so a `kill -9` landing between the
preserve-rename and the publish stranded the file under a pid no
subsequent run would look for, while the tag was left with no
`rootfs.ext4` — or with an unpublished, dirtied clone at that path — under
the old metadata. Unless the tag was republished after the backup was
parked (`snapshot.json` newer than the park), the newest backup is now
renamed back into place; the rest are discarded, except a backup whose
pid is still running a bake.

### Bakes refuse to start on a nearly full disk, and clean up after themselves

Running out of space mid-write does not fail cleanly — it leaves a
*corrupt* artifact: a rootfs whose package files contain other files'
bytes, or a truncated `memory.bin`. Inside a guest that surfaces much
later as `uname: option requires an argument`, `Exec format error`, or
`EBADMSG` on a `/var/lib/dpkg` entry, which reads like a broken build
rather than a broken image. `forkd parent build` (conversion) and
`forkd snapshot` now check free space on the target filesystem first and
refuse with an explicit error below a 5 GiB reserve
(`FORKD_MIN_FREE_GIB` to override). The check is advisory: if `statvfs`
cannot run, it warns and continues rather than blocking work on a broken
measurement.

`forkd snapshot` also removes its staging dir on every exit now, not only
on success. A failure after the volatile artifacts were written — boot
timeout, snapshot error, publish error, interrupt — used to leave a fully
written `memory.bin` (GBs) beside the snapshot dir that nothing ever
collected.

### A failed boot no longer orphans its Firecracker process

`Vm::boot` spawned Firecracker and then issued the `/boot-source`,
`/drives/rootfs`, `/entropy` and `/actions` calls with a bare
`std::process::Child` in hand. `Child` has no kill-on-drop, so any `?` in
that window — including `wait_for_sock` timing out — returned an error and
abandoned a *live* Firecracker. The orphan holds the sandbox's tap device
and rootfs fd, and the tap name is frozen into the vmstate, so every later
spawn of that snapshot dies inside Firecracker with `Open tap device
failed` before it can create its API socket; callers see
`socket .../child-N.sock never appeared` and the snapshot stays unusable
until the orphan is killed by hand. The child is now owned by a
`PendingFirecracker` guard that reaps it on every early return and hands
it over to the `Vm` only on success.

### Rootfs sidecar placement: recorded absolute path, validated

Packs record the rootfs sidecar's target as the vmstate-frozen ABSOLUTE
path again (`manifest.rootfs.target_path`), and `unpack`/`pull` place it
exactly there after validating it sits inside a forkd-managed root (the
forkd data dir, its `snapshots/` tree, or the rootfs cache
`/var/cache/forkd` / `$FORKD_RUN_CACHE`). This restores cross-host
pack/pull behavior for packs built from cache-path bakes and keeps
unpacking under a different `--tag` or data dir restorable (the previous
snap-dir-relative placement put the rootfs where the vmstate wasn't
looking). Paths outside the managed roots — including the brief
relative-path pack format from development builds — are rejected
fail-closed, so a malicious pack still cannot aim a privileged unpack at
arbitrary filesystem locations. A missing or unfetchable sidecar is now
a hard error instead of a warning, since the resulting snapshot could
never be restored. See docs/HUB.md ("Rootfs sidecar").

### Audit-log rotation

The controller now reopens its audit log on `SIGHUP`, with reopen and request
writes serialized under the same writer lock so rotation does not drop or
split records. A successful reopen emits a `log_reopened` audit event. The
systemd unit exposes `systemctl reload`, and the packaged logrotate policy
uses rename/create plus reload instead of `copytruncate`. Closes #278.

### MCP compatibility

Constrain `forkd-mcp` to MCP `>=1.2,<2`: its FastMCP import is unavailable
in older 1.x releases and was removed in MCP 2.x. CI now verifies the stdio
initialize handshake and all registered tools against MCP 1.2.0 and the
latest compatible 1.x release. Closes #275.

## v0.5.3 - 2026-07-22

### KSM / memfd release catch-up

Ships the post-v0.5.2 KSM doctor work from
[#267](https://github.com/deeplethe/forkd/pull/267): `forkd doctor` now
surfaces KSM/memfd guidance, and the probe evidence documents why plain
memfd `MAP_SHARED` is not the right path for #5.

### Daemon spawn and hardening

Adds daemon-backed sandbox spawn, hardens rootfs cleanup around live
mounts, and tightens daemon auth boundaries. This release also carries
the bounded WP-uffd accept and tag/id path-join hardening from the
security follow-up series.

### Packaging and docs

Includes the portable rootfs sidecar hub fixes, Python SDK target parsing
and empty-response handling, branch tag documentation fixes, and updated
quickstart download links for v0.5.3.

## v0.5.2 — 2026-06-08

### Hugepage-backed memfd snapshots — closes #224

Spawned sandboxes can now back their guest RAM with **2 MiB hugepages**
(`MFD_HUGETLB | MFD_HUGE_2MB`) instead of 4 KiB pages, reducing TLB
pressure on spawn-many and live-BRANCH bulk-copy. Opt in per spawn via
`--hugepages` (CLI), `hugepages: true` (REST / SDK). The flag requires
`--live-fork` — only the v0.4 memfd path benefits; file-backed restore
is unaffected.

Plumbed end-to-end by external contributor **@theflashwin** in
[#230](https://github.com/deeplethe/forkd/pull/230) — their first PR to
the project, and unusually complete: Rust `MemoryBackend::MemfdShared`
extended to `{ use_hugepages: bool }`, REST + CLI + Python SDK + TS SDK
all carry the flag, a `forkd doctor` check (`HugePages_Free`) surfaces
pool exhaustion before spawn, ENOMEM falls back to normal pages with a
warning, and `bench/live-fork-pause-window/bench-hugepages.py`
interleaves baseline / hugepages iterations for fair A/B numbers.

The bench harness ships with the PR but **N=100 spawn numbers are still
pending** on a clean host — tracking in #233. PR README has @theflashwin's
N=4 measurements from his hardware.

### `axum-server` 0.7 → 0.8 — closes #192

Drops the unmaintained `rustls-pemfile` dependency flagged by
RUSTSEC-2025-0134. The daemon's TLS path now goes through
`rustls-pki-types` directly. Internal-only change; HTTPS bind / cert
loading on the daemon are byte-compatible. ([#232](https://github.com/deeplethe/forkd/pull/232))

### Non-AI recipes — broadens perceived scope

`recipes/README.md` now leads with a "by problem you're solving" table
that surfaces CI test parallelization, database fixture forking, and
browser farming alongside the AI-agent use case, instead of presenting
forkd as AI-only.

New recipe **`recipes/ci-parallel-pytest`** ships a complete Python
pytest fan-out demo (5 test files, ~30 tests across numpy / pandas /
sklearn / regex / stdlib) backed by a forkd snapshot so each worker
starts hot — `import numpy` etc. is already paid for in the parent.
Bench numbers from the dev box: **~20 ms per worker batch-spawned**
vs ~2-3 s per cold container. ([#231](https://github.com/deeplethe/forkd/pull/231))

## v0.5.1 — 2026-06-05

### Guest kernel rebuild — closes #218 + #225

forkd's pre-v0.5.1 vmlinux was Linux **4.14.174** built 2021-07-14.
That kernel predated `CONFIG_HW_RANDOM_VIRTIO`, `CONFIG_RANDOM_TRUST_CPU`,
the `random.trust_cpu=` cmdline option (Linux 4.19+), and `CONFIG_VMGENID`
(Linux 5.20+). Symptom: `getrandom(2)` blocked forever inside a
restored guest because the CRNG never marked itself initialized —
anything that touched OpenSSL (`pip install`, `urllib.request` HTTPS,
`requests`) hung indefinitely.

**Replacement**: Linux **6.1.141** from Firecracker's CI bucket
(`firecracker-ci/v1.13/x86_64/vmlinux-6.1.141`, sha256
`b36a4a1b10f33b9cfdcde3d1a787d9c090556a3edb211cd06d1f3f9a6c7e8724`).
Verified end-to-end on the dev box on 2026-06-05:

| | before | after |
|---|---|---|
| `uname -r` | 4.14.174 | 6.1.141 |
| `/dev/hwrng` | absent | present (virtio-rng) |
| `getrandom(GRND_NONBLOCK)` | EAGAIN | returns 16 bytes immediately |
| `ssl.create_default_context()` | blocks forever | 0.01 s |
| `urllib.request.urlopen("https://pypi.org/…")` | blocks forever | 0.25 s |
| `pip install numpy==2.0.2` | exit 1, empty output | 21 s, exit 0 |
| **VMGENID on FC restore** | n/a | dmesg: `random: crng reseeded due to virtual machine fork` |

Two new helper scripts:
- [`scripts/install-guest-kernel.sh`](https://github.com/deeplethe/sandstorm/blob/v0.5.3/scripts/install-guest-kernel.sh) — downloads the FC CI vmlinux into `/var/lib/forkd/kernels/vmlinux`; the recommended path.
- [`scripts/build-guest-kernel.sh`](https://github.com/deeplethe/sandstorm/blob/v0.5.3/scripts/build-guest-kernel.sh) — clones Linux 6.1 LTS, applies FC's reference microvm config, builds vmlinux from source (~10 min on a 20-core box). Use this when you want to audit / tune the kernel config or target a non-x86_64 architecture.

**Migration**: existing snapshot vmstates produced under 4.14 still restore on 6.1 — they just don't gain the new entropy device until they're re-baked. Run `forkd from-image <image> --tag <tag>` for each snapshot you care about to pick up CRNG init + `/dev/hwrng` + VMGENID.

`forkd doctor`'s "kernel image" check now suggests running `scripts/install-guest-kernel.sh` when no vmlinux is found.

## v0.5.0 — 2026-06-05

### Diff-snapshot chains

Snapshots can now record a `parent_tag` + content-hash edge to an
earlier snapshot, letting agents stack incremental layers on a
warmed base (`base → +numpy → +pandas → +sklearn`) instead of
producing a flat 1.5-GiB-each snapshot per dependency set. The
daemon walks the chain at spawn time and assembles the memory image
in one pass; correctness verified across depths 1-3 by the Phase 5
bench ([`bench/chain-spawn/RESULTS-v0.5.md`](https://github.com/deeplethe/sandstorm/blob/v0.5.3/bench/chain-spawn/RESULTS-v0.5.md))
with 90/90 (100%) probe passes. Tracking issue
[#216](https://github.com/deeplethe/forkd/issues/216).

- **Phase 1** (PR #213) — `forkd_vmm::chain` library: `resolve_chain`,
  `verify_parent_hashes`, `assemble_chain_memory` with a reflink-
  preferred `cp(base) + apply_diff` walk. 7 unit tests cover cycle
  detection, missing-parent actionable errors, and hash mismatch.
- **Phase 2a** (PR #214) — chain-aware `POST /v1/sandboxes`: when
  the target tag has `parent_tag` set, the controller transparently
  walks the chain, verifies each link's `parent_content_hash`,
  assembles memory into a per-spawn temp file, and FC-restores from
  that. Pre-existing flat-snapshot spawns unchanged.
- **Phase 2b** (PR #215) — `forkd snapshot-diff --from --tag --exec`
  CLI verb. Spawns a one-shot sandbox from the base, runs the exec
  in-guest, then BRANCHes with `parent_tag` set so the resulting
  snapshot records the chain edge. The daemon's BRANCH endpoint
  accepts `parent_tag` on the request body (validated against the
  source's snapshot tag).
- **Phase 5** (PR #217) — chain-spawn bench + a Phase 2a bug fix.
  Headline: on a 512 MiB base (ext4), a depth-3 chain spawn took
  p50 = 1668 ms vs 746 ms for the flat-equivalent. The ~460 ms
  per-link tax tracks SHA-256 of the base — the mmap-once
  incremental verify optimization is queued for v0.6. Same PR also
  fixed a real bug where the chain-assembled `memory.bin` was
  unlinked by `restore_many_with`'s startup sweep before FC could
  load it (caught by running the bench, never by unit tests). New
  regression test `work_dir_sweep_preserves_chainstage_subdirectory`.
- **Phase 4** (PR #219) — chain-aware management verbs:
  - `forkd snapshot-info <tag>` (`GET /v1/snapshots/:tag/info`) —
    chain depth, ancestors, dependents, on-disk sizes.
  - `forkd rmi <tag> [--cascade | --force]` — pre-Phase-4 `rmi`
    silently orphaned chained children; now refuses with HTTP 409 +
    actionable message until the caller picks an escape hatch.
  - `forkd snapshot-compact --from <tag> --to <new>`
    (`POST /v1/snapshots/:tag/compact`) — materialize a chain into a
    new flat snapshot with no `parent_tag`. Stages via
    `.compact-staging-<to>/` + `rename(2)` so a mid-compact crash
    leaves nothing half-written.
- **Phase 3** (PR #220) — Hub pack/unpack bumped to **manifest v2**.
  When the head has `parent_tag.is_some()`, `forkd pack` walks the
  chain on disk and bundles every ancestor under per-link tar
  prefixes; `unpack` materializes one snapshot directory per link.
  Depth-0 (base) snapshots keep emitting v1 packs so older clients
  keep working. Unsafe `chain[].tag` entries (path traversal) are
  rejected at manifest-parse time before any file body is extracted.
- **Phase 6** — docs (this changelog entry, README v0.5 section,
  `docs/API.md` `SnapshotInfoDetail` + new endpoints, `docs/HUB.md`
  pack v2 spec, [`DESIGN-v0.5-diff-snapshot-chains.md`](https://github.com/deeplethe/sandstorm/blob/v0.5.3/DESIGN-v0.5-diff-snapshot-chains.md)).

### v0.4 live-fork: user-facing surface complete

The v0.4 live-fork path — BRANCH that pauses the source for sub-50 ms
instead of the 150-300 ms a Diff BRANCH costs — is now reachable from
every supported entry point. The underlying mechanism (UFFD_WP to
capture dirty pages out-of-band, memfd-backed RAM so Firecracker and
the controller share the same page cache) was prototyped in Phase 6
(PRs #194-#202). This release wires it through the public surface:

- **REST** (PR #204) — `POST /v1/sandboxes/:id/branch` accepts a
  canonical `mode: "full" | "diff" | "live"` field. The legacy `diff:
  true` boolean still works; setting both yields HTTP 400. New
  `wait: false` toggles fire-and-forget (return after the source
  resumes, ~10 ms; snapshot reaches `status: "ready"` asynchronously).
  `POST /v1/sandboxes` accepts `live_fork: true` so the spawned
  sandbox boots with memfd-backed RAM — a prerequisite for later
  `mode: "live"` BRANCHes from it.
- **CLI** (PR #205) — `forkd snapshot --from-sandbox <id> --live`
  and `--live --no-wait`. Clap enforces `--diff` / `--live` mutex
  and `--no-wait` requires `--live`. **Gap**: `forkd fork
  --live-fork` (spawn-time opt-in) isn't surfaced on the CLI yet —
  use the SDK or `POST /v1/sandboxes` directly to spawn a live-fork
  parent. Tracking as a follow-up.
- **SDKs** (PR #206) — Python (`Controller.branch_sandbox(mode=,
  wait=)`, `Controller.spawn_sandboxes(live_fork=)`), TypeScript
  (`branchSandbox({ mode, wait })`, `spawnSandboxes({ liveFork })`,
  `BranchMode` type re-exported), MCP (`branch_sandbox` /
  `spawn_sandboxes` tools). All backwards-compatible: existing
  `diff=True` callers keep working and still drive v0.3.x daemons
  unchanged.
- **Doctor** (PR #207) — two new checks (`uffd_wp (v0.4 live BRANCH)`
  and `memfd_create (v0.4 live BRANCH)`) probe the kernel
  prerequisites. WARN-not-FAIL: v0.3 Diff/Full still work without
  these. Smart hints — EPERM maps to `sysctl
  vm.unprivileged_userfaultfd=1`, ENOSYS to "kernel < 5.7, Diff
  still works".

**Prereqs for `mode: "live"`**: Linux ≥ 5.7 (UFFD_WP),
`unprivileged_userfaultfd=1` or `CAP_SYS_PTRACE`, the vendored
Firecracker fork from
[deeplethe/firecracker:forkd-v0.4-mem-backend-shared-v1.12](https://github.com/deeplethe/firecracker/tree/forkd-v0.4-mem-backend-shared-v1.12)
— upstream FC doesn't yet ship `mem_backend.shared = true`. See
[`docs/VENDORED-FIRECRACKER.md`](https://github.com/deeplethe/sandstorm/blob/v0.5.3/docs/VENDORED-FIRECRACKER.md).

**Bench numbers** ([`bench/live-fork-pause-window/RESULTS-v0.4.md`](https://github.com/deeplethe/sandstorm/blob/v0.5.3/bench/live-fork-pause-window/RESULTS-v0.4.md))
on a clean Hub-pulled `python-numpy` source (1.5 GiB, Intel i7-12700,
ext4 on HDD):

| mode       | pause p50 | pause p90 | RT p50    |
|------------|----------:|----------:|----------:|
| live-sync  |     56 ms |     64 ms | 13 730 ms |
| live-async |     54 ms |    241 ms | **69 ms** |
| diff       |    202 ms |    418 ms | 13 461 ms |
| full       |  13 550 ms |  14 268 ms | 13 559 ms |

Headline: **3.6× faster pause** vs v0.3 Diff at p50, and the gap
widens on slower storage because Live's pause is disk-independent.
`wait=false` gives callers a ~70 ms HTTP return (vs 13.7 s for sync),
**~200× RT improvement** for fire-and-forget BRANCH.

### Security — bearer-token comparison was a length oracle (closes #162)

`crates/forkd-controller/src/auth.rs::constant_time_eq` was advertised
as constant-time but leaked the presented token's length via two
distinct paths:

1. The length-mismatch branch's "fake work" loop used
   `x.wrapping_mul(0)` — the compiler is allowed to (and LLVM does)
   delete the loop as dead code, so response time was monotonic in
   the longer slice's length.
2. Even if the loop had been preserved, taking different branches
   for length-equal vs length-mismatch was itself a timing oracle.

For a deployment with a fixed-length token (e.g. 32-byte hex from
`forkd doctor`), the length oracle collapses security to a single
attempted length, after which the equal-length branch is real
constant-time.

**Fix.** Replaced with a `subtle::ConstantTimeEq` call against a
zero-padded copy of the presented token. Same code path regardless
of input length; length difference is folded into the result via a
non-short-circuiting bitwise AND. `subtle` was already an indirect
dep via the rustls / aws-lc-rs subtree, so the patch adds no new
crate to the lockfile.

Thanks to @m-dmupba for the report and the precise repro.

## 0.3.4 — 2026-05-23

### Multi-BRANCH pause anomaly fixed (closes #146)

The "BRANCH 3-5 pause_ms jumps to 1.3-1.5 s on the same source" anomaly
documented in v0.3 results is gone. Root cause: ext4's delayed
allocation + multi-block allocator + writeback throttle + block-bitmap
checksumming compounded per BRANCH; each BRANCH's 500 MiB+ memory.bin
write triggered increasing ext4 metadata work.

**Fix**: `posix_fallocate` the destination memory.bin to its full size
right after `mkdir snap_dir`, before the diff-mode background `cp` or
Firecracker's `/snapshot/create` write. ext4 reserves the extents
up-front; subsequent writes don't run `ext4_mb_new_blocks` or update
block bitmaps in-band.

**Measured impact** (ext4 SSD, `coding-agent-fork-prewarm-v1` source,
10 consecutive diff BRANCHes):

| BRANCH | 1 | 2 | 3 | 4 | 5 | 6 | 7 | 8 | 9 | 10 |
|---|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|
| before | 350 | 250 | 1300 | 1400 | 1500 | 2700 | 1500 | 1800 | 2700 | 1500 |
| after | 585 | 286 | 344 | 161 | 369 | 153 | 189 | 162 | 324 | 174 |

- BRANCH 6: **17.6× faster** (2700 ms → 153 ms)
- Median across BRANCH 3-10: ~1700 ms → ~200 ms (**~8.5×**)
- After-curve matches a tmpfs control to within noise

Best-effort: on filesystems that don't support `posix_fallocate`
(tmpfs, NFS, FAT, etc.) the call returns ENOSYS, a WARN is logged,
and the BRANCH continues with the previous behavior.

Five rounds of probing produced the diagnosis. Full investigation:
[`bench/pause-window/PROBE-multi-branch-anomaly.md`](https://github.com/deeplethe/sandstorm/blob/v0.5.3/bench/pause-window/PROBE-multi-branch-anomaly.md).

### Other

- `forkd doctor` extended to 14 checks (PR #142 from v0.3.3 — listed
  there but worth re-mentioning since it shipped late in the cycle):
  hw-virt, FC version, docker, snapshot-dir space added.
- CI now auto-dispatches `publish-pypi.yml` from `release.yml`
  via explicit `workflow_dispatch` (PR #144). v0.3.1-v0.3.3 all
  required manual `gh workflow run`; v0.3.4 is the first that
  should auto-publish.
- `forkd snapshot --from-sandbox`, `forkd ls`, `forkd kill`,
  `forkd rmi`, `forkd from-image`, `forkd bench`, `forkd doctor`
  all surface a `branch_count` warning when the source has been
  BRANCHed ≥3 times (less relevant now that the anomaly is fixed,
  but kept for diagnostic clarity).

## 0.3.3 — 2026-05-21

### Six new CLI commands

The `forkd` binary gained a developer-experience cluster:

- **`forkd doctor`** — 10 host-readiness checks (KVM, tap, netns,
  firecracker binary, kernel image, daemon, ...) with PASS / WARN /
  FAIL / SKIP per check and a one-line fix hint for each failure.
  Safe to run unprivileged; skips root-only checks with a note. Use
  this first after a fresh `scripts/setup-host.sh`.
- **`forkd bench`** — a representative spawn → exec → branch(diff=true)
  → fanout → cleanup cycle against the live daemon. Screenshot-friendly
  per-step timing. Answers "is forkd actually fast on this box?" in
  one command.
- **`forkd from-image <docker-image> --tag <tag>`** — Docker pull →
  ext4 → boot + warmup → pause → register tag, in one verb. boxlite
  parity for "`docker pull X` and you're done".
- **`forkd ls`** — list live sandboxes the daemon knows about. Table
  output (id / snapshot / pid / netns / guest_addr).
- **`forkd kill <id>...` / `--all` / `--tag <tag>`** — terminate
  sandboxes via DELETE /v1/sandboxes/:id without hand-writing curl.
- **`forkd rmi <tag>...`** — delete snapshot tags (docker-style).
  Tries daemon DELETE first; falls back to direct disk removal when
  the daemon is unreachable or didn't know the tag.

`forkd images` output also got a table refresh: new MEMORY and
CREATED columns (relative age), most-recent-first sort, snapshot
count + total bytes footer.

### `forkd snapshot --from-sandbox --diff`

The CLI now exposes the v0.3 Diff BRANCH path:

```bash
forkd snapshot --from-sandbox sb-abc-0000 --diff --tag base-plus-pip
# ~200 ms pause, vs multi-second Full mode
```

Closes the last gap from #28 — REST and both SDKs already exposed
`diff`; the CLI was the only one missing.

### Five framework integration recipes

Host-side Python scripts (no rootfs build required) showing how to
plug forkd into:

- **`recipes/mcp-agent/`** — Claude Desktop / Cursor / Cline via MCP
- **`recipes/crewai-fanout/`** — N CrewAI agents on N microVMs
- **`recipes/autogen-branch/`** — AutoGen `CodeExecutor` + mid-conversation BRANCH
- **`recipes/openai-swarm/`** — Swarm/Agents handoff = BRANCH
- **`recipes/speculative-agent/`** — **the headline demo**: BRANCH +
  N strategies + judge picks best. Tweet-friendly artifact (2595×
  faster than slowest strategy in the included example).

Each ships with a `--dry-run` mode that exercises the forkd plumbing
without an LLM key.

### Pause-window anomaly probe (#118 thread-level)

Follow-up to v0.3.1's "BRANCH 3-5 anomaly" finding. Two new probe
scripts and a refined attribution: FC is off-CPU ~94 % of the slow
BRANCH window; the dominant contributors are userspace futex
contention (17/250 in-kernel-sleep samples), ext4 journal IO (~2 %),
and un-symbolized user-space CPU on the snapshot worker thread (FC
static-pie release has no frame pointers; needs DWARF or a debug
rebuild to drill further). See
[`bench/pause-window/PROBE-multi-branch-anomaly.md`](https://github.com/deeplethe/sandstorm/blob/v0.5.3/bench/pause-window/PROBE-multi-branch-anomaly.md).
Direct consequence: original #118 Phase 2/3 scope needs revision.

### Other

- Quick start in both `README.md` and `README-zh.md` rewritten around
  `forkd doctor` + `forkd from-image` + `forkd bench` — the modern
  user path that didn't exist before this release.
- `@deeplethe/forkd` 0.3.1 published to npm (first npm release for
  the TS SDK).

## 0.3.2 — 2026-05-20

Python SDK only. Closes the surface-parity gap between the REST API,
the TypeScript SDK, and the Python SDK:

- `Controller.spawn_sandboxes(prewarm=...)` — opt into the v0.2
  prewarm path that amortizes first-BRANCH cold-cache cost.
- `Controller.branch_sandbox(diff=..., measure_diff=...)` — opt into
  v0.3 Diff BRANCH (and the measurement-only Diff sidecar) from
  Python. REST and TS SDK already had these.

No Rust changes; the workspace stayed at 0.3.1.

## 0.3.1 — 2026-05-19

### Phase 1d: multi-BRANCH diff via the previous-output chain

Lifts the v0.3.0 first-BRANCH-only restriction. The daemon now tracks
`SandboxInfo.last_branch_memory_path` — set to whichever BRANCH most
recently completed (Full or Diff). On the next `diff: true` request:

- Chain head set AND file exists → use it as the cp source. By
  construction it's source's complete state at that BRANCH's pause
  time, which is exactly the base the next diff needs.
- Chain head set BUT file missing (user `DELETE`d an intermediate
  BRANCH) → fall back to `source_tag/memory.bin` with a logged
  warning. Lossy but doesn't crash.
- Chain head unset (first BRANCH on sandbox) → use `source_tag/memory.bin`
  as before.

Zero extra storage (each BRANCH's output is already on disk; we just
point at it). No background tasks. No separate shadow file.

The `has_branched: bool` flag stays in `SandboxInfo` as a diagnostic;
the daemon no longer 400s on it. The previously-shipped 400 error
message for second-and-later diff BRANCHes is gone.

Measurement: 3 trials × 5 consecutive `diff: true` BRANCHes on a
mem-2048 SSD source. All 15 succeed. Diff sizes stay <1.2 MB per
BRANCH (bitmap-clear semantics confirmed). Aggregate source downtime
across the 5 BRANCHes is ~4.7 s vs ~70 s if these had been Full —
**14× pause-window reduction over a multi-BRANCH workflow.** Raw
data in `bench/pause-window/multi-branch-sweep.csv`; new sweep script
`bench/pause-window/sweep-multi-branch.sh`.

Anomaly noted but not blocked-on: pause_ms jumps from ~280 ms at
BRANCH 1-2 to ~1.3-1.5 s at BRANCH 3-5 despite the diff size staying
small. Likely a KVM / firecracker control-plane accumulating cost;
filed for follow-up. Still ~10× better than Full mode at every cell.

## 0.3.0 — 2026-05-19

**Headline: source-pause window for BRANCH drops 6-143× depending on workload.**
Idle 4 GiB SSD source: 29 s → 205 ms = 143×.
Typical agent workload (30-300 MiB dirty footprint on 2 GiB source): 6-15×.
Crossover at ~65 % source RAM dirty. Honest curve and practical
guidance in
[`bench/pause-window/RESULTS-v0.3.md`](https://github.com/deeplethe/sandstorm/blob/v0.5.3/bench/pause-window/RESULTS-v0.3.md).

This release also bundles v0.2.5's prewarm fix (#100) — variance
reduction for BRANCH pause on cold-cache hits, sandbox-creation
trade-off — and the v0.3-cycle scaffolding for the deferred live-fork
plan (#101, kept as honest record + revival starting point).

### v0.3 phase 1: diff snapshots — 4 GiB SSD source pause 29 s → 205 ms (143×)

- **`Vm::snapshot_diff_to`** in `forkd-vmm` — calls Firecracker
  `/snapshot/create` with `snapshot_type: "Diff"`, returns a
  `DiffSnapshot` carrying both logical and physical sizes (the latter
  = on-disk allocated bytes = the BRANCH's dirty footprint).
- **`apply_diff(diff_path, base_path)`** helper — `SEEK_DATA`/
  `SEEK_HOLE` walk the diff's allocated extents, 1 MiB chunks copied
  onto the same offsets of the base file. Returns bytes copied.
  Linux-only; non-Linux builds bail rather than silently degrading.
- **`ForkOpts.enable_diff_snapshots: bool`** — required on
  `/snapshot/load` for the resulting VM to admit Diff snapshot/create
  calls. Default false (v0.2 callers preserve identical behavior);
  daemon's `create_sandbox` flips it to true for all daemon-spawned
  sources.
- **`POST /v1/sandboxes/:id/branch` gains `"diff": bool`.** When true,
  the daemon parallelizes the source-tag memory.bin copy with the
  source running, takes a Diff snapshot during a ~200 ms pause,
  resumes the source, joins the copy, and merges the diff onto the
  pre-copied output. The user-visible pause is just the Diff window —
  source TCP connections, kvmclock, and timers see a ~200 ms gap
  instead of seconds. Total BRANCH API latency is unchanged on SSD
  (still bandwidth-bound on the cp); only source DOWNTIME shrinks.
- **`SandboxInfo.has_branched: bool`** + `Registry::mark_branched()`
  gate that rejects second-and-later `"diff": true` BRANCHes with a
  clear 400. Firecracker clears the dirty bitmap on every
  snapshot/create, so a second Diff would silently miss pages dirtied
  before BRANCH 1. Multi-BRANCH diff support (per-sandbox shadow
  file) is deferred to v0.3.1+; forkd's canonical "spawn → BRANCH
  once → fan out N → discard source" workflow only ever takes one
  BRANCH per sandbox, so the restriction covers ~80% of use cases.
- **`SnapshotInfo`** gains `diff_ms`, `diff_physical_bytes`,
  `diff_logical_bytes` — populated when `diff: true` or
  `measure_diff: true` was set on the BRANCH request.
- **Measurement**:
  [`bench/pause-window/RESULTS-v0.3.md`](https://github.com/deeplethe/sandstorm/blob/v0.5.3/bench/pause-window/RESULTS-v0.3.md)
  with the full A/B (5 memory sizes × 3 trials × 2 modes × 2 backends
  = 60 trials). Phase 1a numbers (sidecar Diff inside the existing
  Full pause) match phase 1b numbers (real `diff: true`) within
  measurement noise — architecture validated. Honest caveats: idle-
  source best case; 256 MiB on tmpfs is a wash (control-plane floor
  exceeds memcpy); first-BRANCH-only restriction.
- **Sweep scripts**: `sweep-diff.sh` (phase 1a sidecar timing) and
  `sweep-diff-real.sh` (phase 1b real A/B). Raw data CSVs checked in.

### v0.3 scaffolding (deferred — kept as honest record)

> **Deferred to v0.4+.** Live-fork via memfd + uffd_wp is tracked in
> [issue #101](https://github.com/deeplethe/forkd/issues/101). The
> architecture has an open question on source-divergence sync that we
> haven't sketched concretely enough to commit to weeks of Firecracker
> maintenance for. v0.3 is now pursuing cheaper pause-window wins —
> diff snapshots, NVMe + io_uring, pre-emptive background snapshot —
> see [`docs/ROADMAP.md`](./docs/ROADMAP.md). The scaffolding below
> stays in the repo because it's reusable when/if the project picks
> the live-fork work back up.

- **`MemoryBackend::Userfault` enum variant** in `forkd-vmm`,
  reserved for the (now-deferred) live-branching design. Setting it
  today errors out of `restore_many_with` with a pointer to
  [`docs/design/userfaultfd.md`](https://github.com/deeplethe/sandstorm/blob/v0.5.3/docs/design/userfaultfd.md); no
  caller can accidentally rely on a behavior we haven't built.
- **`forkd-uffd` crate, phase 1.** New workspace member containing the
  library half of the userfaultfd page-fault handler. Implements
  Firecracker's UDS handshake: `recvmsg` with `SCM_RIGHTS` to receive
  the uffd file descriptor + a JSON-encoded `Vec<GuestRegionUffdMapping>`
  describing the host VAs of guest memory regions. Wire-compatible with
  Firecracker v1.10.1's `src/firecracker/examples/uffd/uffd_utils.rs`.
  Ships a `forkd-uffd-handler` binary that accepts the handshake and
  exits — no `UFFDIO_COPY` event loop. Round-trip handshake test
  paired over `socketpair(2)` so CI exercises the parser without
  needing a real Firecracker. Reusable as-is if the live-fork plan
  revives; orthogonal value as a reference implementation of the
  Firecracker uffd protocol.
- **`firecracker-patch/` directory — REMOVED.** Originally drafted
  a ~100 LOC patch for a `MemoryBackend::Memfd` Firecracker extension
  (forking upstream at v1.10.1). After v0.3 phase 1 shipped 143× on
  vanilla Firecracker, we evaluated whether to actually take the fork
  path and decided not to: the memfd value-add doesn't add sharing
  capability we don't already have via `mmap MAP_PRIVATE`, and the
  fork-maintenance cost (own CI, rebase on every upstream tag, track
  CVEs, weakened trust story) isn't justified for the remaining
  pause-window headroom. Reasoning in
  [`docs/design/userfaultfd.md`](https://github.com/deeplethe/sandstorm/blob/v0.5.3/docs/design/userfaultfd.md) §
  "Why we won't fork Firecracker"; revival criteria in
  [issue #101](https://github.com/deeplethe/forkd/issues/101).

### Features

- **Sandbox prewarm: amortize the cold-cache penalty at create time.**
  New `"prewarm": true` field on `POST /v1/sandboxes`. When set, the
  daemon performs a throwaway snapshot to scratch storage
  (configurable, default `/dev/shm/forkd-prewarm`) immediately after
  each child is restored, faulting in all guest pages and populating
  KVM EPT. After prewarm, the first user-visible BRANCH runs at
  steady-state speed instead of paying the measured 2-9x cold-cache
  penalty (see
  [`bench/pause-window/RESULTS-v0.2.md`](https://github.com/deeplethe/sandstorm/blob/v0.5.3/bench/pause-window/RESULTS-v0.2.md)).
  The cold cost is **moved**, not eliminated: sandbox creation pays
  one cold-pause-window worth of latency in exchange for a consistent
  BRANCH latency from the first call. Useful when BRANCH is the
  request handler with an SLO; not useful for create-then-one-BRANCH
  end-to-end latency. Off by default — opt in via the request body.
  Implemented in `forkd-vmm` as `Vm::prewarm()` +
  `ForkOpts::prewarm_scratch_dir`; daemon adds `--prewarm-scratch-dir`
  flag / `FORKD_PREWARM_SCRATCH_DIR` env var.
- **forkd Hub MVP**. `forkd pull <owner>/<name>` resolves through a
  registry.json published in this repo and downloads
  `.forkd-snapshot.tar.zst` packs from GitHub Releases. sha256-
  verified, optional `@<version>` pinning, free public hosting,
  no per-package config. Full spec + publish workflow:
  [`docs/HUB.md`](./docs/HUB.md). First published pack:
  `deeplethe/langgraph-react` (14.5 MiB after 35× zstd
  compression).
- **CLI `forkd pull` rewrite** to use the new registry indirection
  with sha256 integrity check, replacing the previous string-build
  approach against `forkd-hub.deeplethe.com` (which we never set
  up the DNS for).

### Demos / recipes

- **`recipes/langgraph-react/`** — branch-and-fan-out demo of a
  real LangGraph-style ReAct agent. Source agent runs 2 ReAct
  steps with tool calls, BRANCH pauses it (~4 s on SATA SSD,
  ~160 ms on tmpfs), 3 grandchildren
  spawn with different steering hints, each produces a different
  itinerary inheriting the same prior cognitive state. Full writeup
  + asciinema cast embedded in README + first real-run artifacts
  at [`recipes/langgraph-react/results-2026-05-18/`](./recipes/langgraph-react/results-2026-05-18/).
- **`recipes/coding-agent-fork/`** — the "why not parallel prompt?"
  rebuttal. 50 MiB binary blob travels byte-identically across 4
  sandboxes through a single BRANCH; three grandchildren each
  apply different fix strategies (sed / rewrite / skip-tests) and
  produce visibly different outcomes. Artifacts at
  [`recipes/coding-agent-fork/results-2026-05-19/`](./recipes/coding-agent-fork/results-2026-05-19/).
- **`recipes/cube-langgraph/`** (stub) — design sketch for
  CubeSandbox + forkd side-by-side deployment. Pairs with
  [`docs/INTEGRATION-CUBESANDBOX.md`](./docs/INTEGRATION-CUBESANDBOX.md)
  which compares the two projects honestly and proposes 3 concrete
  integration patterns.

### Infrastructure

- **tmpfs `/tmp` mount in `forkd-init.sh`**. Per-VM 256 MiB tmpfs
  prevents the shared-rootfs corruption that hit the langgraph-react
  demo when 3 grandchildren wrote concurrently to the same on-disk
  inode. Affects every recipe built after this commit; no API
  change. **Always put writable demo state under `/tmp`.**

### Benchmarks

- **`bench/pause-window/RESULTS-v0.2.md`** — first-cut measurement
  shows pause is storage-bound: **163 ms ± 7 ms on tmpfs**
  (4 trials), **4.26 s ± 0.41 s on SATA SSD** (5 trials) for the
  same 513 MiB source. Same forkd code; only `--snapshot-root`
  differs. 5/5 connection survival, 0 in-flight loss across SSD
  trials. Surprising mechanism:
  in-guest agents are pause-blind because kvmclock's monotonic
  catch-up on resume races recv data delivery; the recv returns
  data before its timeout timer can fire. The userfaultfd bet's
  value sharpens to "external observers see the pause; in-guest
  agents barely notice".

## 0.1.4 — 2026-05-17

### Security

- **`create_sandbox` snapshot_tag validation gap** (MEDIUM-HIGH,
  post-auth, fixed in PR #54). `POST /v1/sandboxes` accepted
  `req.snapshot_tag` from the request body and joined it directly
  into `snapshot_root` without calling `is_safe_tag` — unlike sister
  handlers `delete_snapshot` and `branch_sandbox` which both
  validated. The unvalidated tag also persisted into
  `SandboxInfo.snapshot_tag` and later flowed into
  `read_snapshot_volumes` during BRANCH, which `serde_json::from_str`'d
  the file at `<snapshot_root>/<tag>/snapshot.json` — letting an
  authenticated attacker control volume mounts of grandchild VMs.
  Full advisory:
  [v0.5.3 security policy → 2026-05-17](https://github.com/deeplethe/sandstorm/blob/v0.5.3/docs/SECURITY.md#past-advisories).
- **K8s manifest placeholder bearer token**. The shipped
  `packaging/k8s/forkd-controller.yaml` ships `token: REPLACE_ME_*`.
  Users who forget to `sed` it before `kubectl apply` would get a
  daemon protected only by a publicly-known token. Fixed: daemon
  refuses to start if the token begins with `REPLACE_ME` /
  `CHANGE_ME` or is shorter than 16 bytes.
- **`boot_wait_secs` cap**. `POST /v1/snapshots` previously accepted
  any `u64`. Clamped to 60 s.

### Reliability

- **BRANCH concurrency caps** (PR #56). Two `POST /branch` calls on
  the same target tag now serialise via a per-tag in-flight set
  (second gets 409) — previously both could pass the
  `vmstate.exists()` TOCTOU and clobber each other. The daemon also
  admits at most `DEFAULT_BRANCH_CONCURRENCY` (4) BRANCHes
  simultaneously; excess gets 503. Both bounds use an RAII
  `BranchSlot` guard so every error path releases cleanly.

### Observability

- **`pause_ms` on BRANCH responses** (PR #58). `SnapshotInfo` now
  carries an optional `pause_ms` populated by `branch_sandbox` with
  the measured `pause() → resume()` envelope on the source VM.
  Also emitted as a structured `tracing::info!` event. Powers the
  new `bench/pause-window/` harness.

### Benchmarks

- **Pause-window harness + first-cut results**. New
  `bench/pause-window/` directory with a synthetic ping/pong agent,
  host-side echo server, orchestrator, and pure-function analyzer
  (14 unit tests, covered by a new `bench-python` CI job). 5
  trials on real hardware (513 MiB source): mean pause
  **4.26 s ± 0.41 s**, **0 in-flight loss, 5/5 connection survival**.
  Surprising mechanism (in-guest agents are nearly pause-blind via
  kvmclock catch-up) documented in
  [`bench/pause-window/RESULTS-v0.2.md`](https://github.com/deeplethe/sandstorm/blob/v0.5.3/bench/pause-window/RESULTS-v0.2.md).

## 0.1.3 — 2026-05-14

### Security

- **Path traversal via `--tag`** (CVE-class, fixed). `forkd snapshot`,
  `forkd unpack`, `forkd pull`, `forkd fork`, `forkd pack`, and
  `forkd push` accepted arbitrary strings for `--tag` and used them
  in `Path::join`, which silently discards the base when the right
  side is absolute. A tag like `/etc/forkd-bad` or `../../etc/x`
  could write Firecracker snapshot files outside the data directory.
  The same risk extended to the `tag` field of `manifest.toml`
  inside a Snapshot Hub pack — a malicious or compromised pack
  could write its files anywhere the running user can write.
  Affects 0.1.0–0.1.2. Fixed by validating tags against
  `[A-Za-z0-9_][A-Za-z0-9._-]{0,63}` at every CLI surface and again
  on the manifest's `tag` field. Full advisory:
  [v0.5.3 security policy → Past advisories](https://github.com/deeplethe/sandstorm/blob/v0.5.3/docs/SECURITY.md#past-advisories).
- `forkd cleanup` would mis-classify live VMs as "safe to delete"
  because `lsof` returns empty stdout (only warnings on stderr) for
  Firecracker UNIX domain API sockets. Under `forkd cleanup --yes`
  this would have torn down the work_dir of an actively running
  VM. Replaced the detection with a `/proc/<pid>/fd/*` readlink
  scan that explicitly checks whether any process holds an open
  handle inside the candidate directory.

### Added

- `forkd push <local-tag> <url>` — HTTP PUT a packed snapshot to
  any URL (presigned PUT from R2/S3/etc. is the intended fit).
- `forkd cleanup` — sweep orphan `/tmp/forkd-{fork,parent,unpack,
  pull}-*` work directories left behind by crashed or killed
  runs. Dry-run by default; `--yes` to actually delete. Refuses
  to touch directories whose `/proc` fd scan shows a live process.
- `scripts/netns-teardown.sh` — reverse `netns-setup.sh`.
  Dry-run by default, removes only `^forkd-child-[0-9]+$` netns.
  Docker bridges, system tap, and `forkd-br0` are untouchable
  without explicit `--include-bridge` / `--include-tap` flags.
- `forkd snapshot --mem-size-mib` — override the parent VM
  memory size (default 512 MiB). Required for memory-hungry
  warmup workloads; browser recipes need ≥ 2048 MiB to avoid
  Chromium OOM during snapshot.
- `forkd snapshot --keep-workdir` / `forkd fork --keep-workdir` —
  preserve `/tmp/forkd-{parent,fork}-<tag>/` after a successful
  run for post-mortem inspection. The default behaviour now
  removes the work_dir on success (failure paths still preserve
  it).
- Pre-flight check on `forkd snapshot` / `forkd fork` refuses to
  start when another forkd run on the same tag is already in
  flight (live process holding sockets in the same work_dir), and
  cleans stale work_dirs from earlier crashes before proceeding.
- Snapshot Hub: pack-and-go via `forkd pack` / `forkd unpack` /
  `forkd pull`. Manifest records per-file sha256, format version,
  and a reserved `parent_tag` slot for the M2.1 diff-snapshot
  chain work.
- `recipes/playwright-browser/` — fork a warmed headless Chromium
  parent. Each child VM inherits a fully-initialised browser via
  mmap CoW; per-call `sb.eval("await page.title()")` returns in
  ~10–80 ms instead of the ~2 s required for a cold Chromium
  spawn. Requires `--mem-size-mib 2048`.
- `recipes/jupyter-kernel/` — SciPy-warm parent for code-
  interpreter workloads.
- `forkd-agent.py` recipe-level eval bridge. When the rootfs
  contains `/etc/forkd-recipe.env` declaring `FORKD_WARMUP_CMD`
  and `FORKD_AGENT_LANG=node`, the agent multiplexes
  `sb.eval(<js>)` calls to a warmup subprocess over a
  line-JSON protocol. `Sandbox.eval()` deserialises the reply
  back into a native Python object.
- `ROADMAP.md` documenting M1 / M2 / M3 milestones.
- README Snapshot Hub section, Chinese translation
  (`README-zh.md`), PyPI version badge.

### Changed

- `forkd eval` now prints the `result_json` field returned by
  Node-recipe replies; previously this surface was silently
  dropped on the CLI side. Python recipes' `result` (repr-string)
  path unchanged for backwards compatibility.
- Warmup process inside `playwright-browser` emits sentinel
  strings (`__js_Infinity__`, `__js_-Infinity__`, `__js_NaN__`)
  for non-finite JavaScript numbers, which `JSON.stringify`
  otherwise silently converts to `null`. Takes effect for any
  recipe rebuilt against 0.1.3.
- Error messages on `forkd unpack`, `forkd pull`, integrity
  failures, manifest parse errors, and HTTP failures now show
  the underlying `Caused by:` chain with operator-actionable
  hints (DNS failure, expired presigned URL, corrupted pack,
  etc.).

### Internal

- `crates/forkd-cli`: new `hub` module for pack format + push/pull.
- `rootfs-init/tests/` — host-runnable smoke tests for the
  recipe eval bridge (`fake-warmup.py`, `smoke-test.sh`,
  `smoke-sdk.py`).
- CI: branch-protected `main` with `rust` job (fmt + clippy +
  test) required; PyPI Trusted Publisher (OIDC) workflow.

## 0.1.2 — 2026-05-12

- Python SDK published to PyPI (`pip install forkd`).
- CubeSandbox row in the README benchmark table now leads with
  the bare-metal-host context after a `systemd-detect-virt: none`
  proof, to address the "nested virtualisation might be skewing
  the numbers" concern raised upstream.

## 0.1.1 — 2026-05-11

- README "Where forkd fits" rewritten with 5 concrete use cases.
- Initial GitHub Release pipeline.

## 0.1.0 — 2026-05-10

- Initial public release. Fork-on-write microVM primitive,
  controller daemon, REST API, Python SDK, six recipes, and
  the N=100 spawn benchmark.
