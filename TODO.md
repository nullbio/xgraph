# xgraph daemon lifecycle and routing TODO

- [x] Add explicit `project_root` routing for every MCP tool call.
- [x] Add CLI `--project-root` routing while preserving cwd as the default.
- [x] Route each MCP tool call to the daemon for its canonical Git worktree root.
- [x] Reconnect and retry once when a cached daemon socket is stale.
- [x] Start the daemon before CLI/MCP graph commands when no daemon is running.
- [x] Keep startup locking so concurrent proxies start at most one daemon per worktree.
- [x] Include the routed project root at the top of every rendered tool response.
- [x] Shut down daemons after 15 minutes without received commands and no in-flight command.
- [x] Shut down daemons when the worktree root or persistent xgraph store path disappears.
- [x] Keep proxy processes tied to agent stdio so they exit when the agent exits.
- [x] Cover lifecycle/routing behavior with focused regression tests.
