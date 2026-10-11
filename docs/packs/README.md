# Command packs

A command pack gives Shimmer's commands themed names. Each alias stands for `shimmer` **and** a
whole command:

```
$ shimmer packs use anime-tropes
✓ active pack: anime-tropes (Anime Tropes)
  linked 50 commands into /home/me/.local/bin
try it: ohayo

$ ikuzo deep-work            # = shimmer workspaces activate deep-work
$ yatta leetcode/two-sum     # = shimmer records complete leetcode/two-sum
```

Packs are optional. With no pack on, Shimmer is plain `shimmer …`, and the real names always work
whichever pack is on. The design and every rule are in
[ADR 0013](../decisions/0013-command-packs.md).

## Using a pack

| Command | What it does |
|---|---|
| `shimmer packs list` | Every pack, built in or yours, and which one is on |
| `shimmer packs show NAME` | A pack's aliases and the command each one runs |
| `shimmer packs use NAME` | Turn a pack on; its aliases work on their own (`ikuzo …`) and after `shimmer` |
| `shimmer packs use NAME --no-link` | Turn it on, but aliases only work after `shimmer` (`shimmer ikuzo …`) |
| `shimmer packs use none` | Turn packs off; the commands they added are removed |
| `shimmer packs link` / `unlink` | Add the active pack's commands again, or remove them |
| `shimmer --pack NAME …` | Use a pack for one command, without switching |
| `shimmer --help` | Shows the active pack's aliases; `--help --canonical` leaves them out |

Your choice is kept in `~/.config/shimmer/cli.toml` (or `$XDG_CONFIG_HOME/shimmer/cli.toml`). It
belongs to this machine, so copying your Shimmer folder elsewhere doesn't bring it along.

## The built-in packs

| Command | `anime-tropes` | `ship-it` | `starship` | `short` |
|---|---|---|---|---|
| ping | `ohayo` | `on-call` | `comms` | `up` |
| shutdown | `owari` | `ooo` | `cryo` | `down` |
| manifest | `nakama` | `readme` | `blueprints` | `ops` |
| records collections | `iroiro` | `boards` | `fleets` | `rcol` |
| records add | `kohai` | `ticket` | `waypoint` | `radd` |
| records list | `mite` | `triage` | `logbook` | `rlist` |
| records get | `naruhodo` | `blame` | `scan` | `rget` |
| records update | `senpai` | `amend` | `retrofit` | `rset` |
| records complete | `yatta` | `lgtm` | `landed` | `rdone` |
| records reopen | `mou-ikkai` | `regression` | `re-entry` | `rreopen` |
| records rename | `kaimei` | `rebrand` | `redesignate` | `rmv` |
| records remove | `sayonara` | `wontfix` | `airlock` | `rrm` |
| records restore | `okaeri` | `revert` | `salvage` | `rundo` |
| workspaces list | `minna` | `envs` | `sectors` | `wlist` |
| workspaces status | `nani` | `standup` | `diagnostics` | `wst` |
| workspaces activate | `ikuzo` | `deploy` | `engage` | `wgo` |
| workspaces cleanup | `daijoubu` | `postmortem` | `damage-control` | `wclean` |
| workspaces force-relaunch | `yatte-yaru` | `force-push` | `override` | `wforce` |
| workspaces reset | `tadaima` | `rollback` | `cold-start` | `wreset` |
| queue list | `mada-mada` | `pipeline` | `launch-queue` | `qlist` |
| queue show | `misete` | `job-status` | `telemetry` | `qget` |
| queue cancel | `yamete` | `cancel-build` | `abort` | `qcancel` |
| queue move | `hayaku` | `bump` | `clearance` | `qmove` |
| scheduler list | `jaa-ne` | `cron-jobs` | `flight-schedule` | `slist` |
| scheduler add | `kimeta` | `schedule-it` | `plot-course` | `sadd` |
| scheduler pause | `chotto-matte` | `code-freeze` | `hold-position` | `spause` |
| scheduler resume | `hajime` | `thaw` | `full-ahead` | `sresume` |
| scheduler remove | `mou-ii` | `deprecate` | `scrub-mission` | `sdel` |
| records templates | `sugoi` | `boilerplate` | `starcharts` | `rtpl` |
| records new | `yosh` | `greenfield` | `commission` | `rnew` |
| records trash | `gomen` | `graveyard` | `debris-field` | `rtrash` |
| records purge | `itai` | `rm-rf` | `self-destruct` | `rpurge` |
| records import | `irasshaimase` | `migrate` | `tractor-beam` | `rimport` |
| records export | `ganbatte` | `dump` | `transmit` | `rexport` |
| records check | `uso` | `lint` | `sensor-sweep` | `rcheck` |
| records rename-field | `henshin` | `refactor` | `recalibrate` | `rmvfield` |
| records rename-collection | `kakkoii` | `pivot` | `rename-fleet` | `rmvcol` |
| records remove-collection | `oyasumi` | `archive` | `mothball` | `rrmcol` |
| records restore-collection | `okaerinasai` | `unarchive` | `recommission` | `rundocol` |
| workspaces stop | `otsukare` | `eod` | `dock` | `wstop` |
| workspaces remove | `mata-ne` | `decommission` | `stand-down` | `wrm` |
| workspaces restore | `hisashiburi` | `cherry-pick` | `homecoming` | `wundo` |
| workspaces rename | `dare-da` | `rename-env` | `callsign` | `wmv` |
| workspaces copy | `bunshin` | `fork` | `sister-ship` | `wcp` |
| workspaces reconfigure | `chotto` | `hotfix` | `refit` | `wset` |
| workspaces edit | `mendokusai` | `monkey-patch` | `engineering` | `wedit` |
| workspaces templates | `kawaii` | `starter-kits` | `shipyard` | `wtpl` |
| workspaces peek | `hontou` | `code-review` | `long-range-scan` | `wpeek` |
| workspaces new | `yoroshiku` | `scaffold` | `maiden-voyage` | `wnew` |
| scheduler show | `itsu` | `eta` | `countdown` | `sget` |

## Writing your own

A pack is a folder in `packs/` inside your Shimmer folder (`$SHIMMER_HOME`, by default
`~/.local/share/shimmer`, or `~/Library/Application Support/shimmer` on macOS), holding one
`pack.toml`. Start from the example:

```
cp -r docs/packs/my-first-pack ~/.local/share/shimmer/packs/
```

```toml
[pack]
id = "my-first-pack"          # must match the folder name
label = "My First Pack"       # shown in 'shimmer packs list'

[alias]
"hello"    = "core.ping"
"focus"    = "workspaces.activate"
"finished" = "records.complete"
```

On the right of each alias goes the command it runs, by its op name:

| Op | Runs |
|---|---|
| `core.ping`, `core.shutdown`, `core.manifest` | `shimmer ping`, `shutdown`, `manifest` |
| `records.collections`, `.add`, `.list`, `.get`, `.update`, `.complete`, `.reopen`, `.rename`, `.remove`, `.restore`, `.trash`, `.purge`, `.import`, `.export`, `.check`, `.templates` | `shimmer records <verb>` |
| `records.create_collection` | `shimmer records new` |
| `records.rename_field`, `.rename_collection`, `.remove_collection`, `.restore_collection` | `shimmer records rename-field`, `rename-collection`, `remove-collection`, `restore-collection` |
| `workspaces.list`, `.status`, `.activate`, `.cleanup`, `.reset`, `.stop`, `.remove`, `.restore`, `.rename`, `.copy`, `.reconfigure`, `.edit`, `.templates`, `.peek` | `shimmer workspaces <verb>` |
| `workspaces.force_relaunch`, `workspaces.create` | `shimmer workspaces force-relaunch`, `new` |
| `queue.list`, `.task`, `.cancel`, `.reorder` | `shimmer queue list`, `show`, `cancel`, `move` |
| `scheduler.list`, `.get`, `.add`, `.pause`, `.resume`, `.remove` | `shimmer scheduler list`, `show`, `add`, `pause`, `resume`, `remove` |

`shimmer packs --help` prints the same list. Your own pack names as many or as few as you like;
the built-in packs name every one.

Then check it, and turn it on:

```
$ shimmer packs check my-first-pack
✓ my-first-pack: 7 aliases, no problems
$ shimmer packs use my-first-pack
```

`shimmer packs check` lists every problem at once. The rules:

- `id` matches the folder name. `id` and every alias start with a lowercase letter, use only
  `a-z`, `0-9`, `-` and `_`, and are at most 32 characters. `label` is one line, at most 40.
- An alias can't be one of Shimmer's own command words (`ping`, `records`, `workspaces`, `packs`,
  `help`, …, plus `queue`, `scheduler`, `calendar` and a few others kept for commands to come).
- A pack has 1 to 64 aliases. Several aliases may run the same command.
- Unknown keys are errors, so a typo is caught rather than ignored.
- Packs are data only: no scripts, ever.

A folder with the same name as a built-in pack replaces it.

If your pack ever breaks, Shimmer doesn't: it prints one warning and runs with the real names,
and `shimmer packs check NAME` tells you what to fix.

## Commands on their own: where they go, and what's skipped

`shimmer packs use` puts a link per alias in `~/.local/bin` (or `link_dir` in `cli.toml`), pointing
at the `shimmer` program. That folder must be on your `PATH`; if it isn't, Shimmer says so.

It is careful with that folder:

- It never overwrites anything. If a name is already taken there, already a program elsewhere on
  your `PATH`, or a common tool name (`git`, `go`, `rg`, …), that alias is skipped and named in the
  output. It still works as `shimmer <alias>`.
- It only ever removes links it made itself, and only while they are still links.

If you switch packs by editing `cli.toml` by hand, an old alias tells you to run
`shimmer packs link`, which tidies up.

## Sharing a pack

A pack is just a folder: share it as a git repo, a gist or a zip, and others copy it into their
`packs/`. Packs built into Shimmer itself must be original: no names from someone else's game,
show, film or brand.
