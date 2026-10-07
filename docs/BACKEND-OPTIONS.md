# Execution backend

The current backend is pty-process 0.5.3 plus one independent same-binary broker. pty-process provides Linux PTY spawn/input/output/resize; broker.rs owns explicit execution records, pipe readers, bounded event journal and signals. It does not create an implicit shell or maintain a second JOBS map. Public APIs and limits are in [README](../README.md); Events are in [EVENTS](EVENTS.md).

The earlier 2026-10-06 shpool probe built libshpool/shpool 0.11.5 and protocol 0.4.3 at source revision `3c41df9a610428b6c1766d78d36d3fefd5685c3b`, exercising real headless attach/stdin/output/reattach/exit/stop. Its [historical probe log](../evidence/shpool-spike.log) is preserved. It showed useful retained sessions, but shpool's shell/session policy and protocol adapter added work to a small explicit argv execution API. No shpool/tmux dependency is required here.

The broker survives frontend replacement and holds the actual child handles. Broker/host restart loses executions and RAM history; project files and private Events subscription state remain on disk. A new broker epoch rejects old execution IDs and reports cursor loss. No per-session keeper, forced worktree or broker-restart process recovery is implemented. Detached descendants remain project-managed. Current evidence is [VALIDATION](../VALIDATION.md); the shpool probe is not proof of this broker's behavior.

The local-mcp derived file/sandbox code is separately attributed in [UPSTREAM](UPSTREAM.md) and [NOTICE](../NOTICE.md).
