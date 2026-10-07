# Persistent terminal backend selection

[nakasyou/local-mcp](https://github.com/nakasyou/local-mcp) 由来のツール・承認・sandboxを基礎として、ここに記載する保持PTY/brokerはdev-session-mcp側の追加実装です。upstream自体の機能や選定として扱いません。[由来と保守範囲](UPSTREAM.md)、[NOTICE](../NOTICE.md)。

2026-10-06 UTC。採用済みbackendは **pty-process 0.5.3 + 同一binaryの独立PTY broker** です。tmux/shpoolはruntime依存にしません。通常APIはsession_idによるrun_command/read_output/send_stdin/resize_command/stop_commandです。

## libshpoolの実probe

[shell-pool/shpool](https://github.com/shell-pool/shpool) revision `3c41df9a610428b6c1766d78d36d3fefd5685c3b`、libshpool/shpool 0.11.5、protocol 0.4.3をLinuxの一時HOME/private socketで実buildし、headless attach、stdin/output、detach/reattach、exit 7、別session、named stopを確認しました。[probeログ](../evidence/shpool-spike.log) を保存しています。

切断中に終了したcommandへ同じ名前でattachすると新commandがCreatedとなり、counterが2になりました。最終状態の取得でcommandを再実行する挙動はMCP jobの契約に合わないため採用しません。これは実probeで確認した結果です。ライブラリ全般の欠陥や全ケースの出力欠落と断定しません。

公開 [libshpool API](https://docs.rs/libshpool/latest/libshpool/) はArgs/Commands/run/Hooks中心で、[runの安全条件](https://docs.rs/libshpool/latest/libshpool/fn.run.html) はthread開始前のforkを要求します。また[開発文書](https://github.com/shell-pool/shpool/blob/master/HACKING.md) は内部protocolのsemver互換を保証しません。MCPのjob結果を安定保持するには追加adapterが必要です。

## 採用した最小構成

[pty-process](https://docs.rs/pty-process/latest/pty_process/) のasync機能でPTY生成、child process、stdin/stdout、resizeを使います。このcrateは完成済みsession poolではありません。brokerのprivate socket、session/job対応、出力上限、終了結果、stdin routing、個別停止、metadata再発見はこちらの追加実装です。

brokerは作業UIDの別processとして自動起動し、frontend再起動から独立してPTYとjobごとの直近64 KiB、status/exit codeを保持します。socketは0600、双方で同一UIDを確認し、frameと応答時間を制限します。終了時は最終出力をdrainしてからcompletedを公開します。stopはjobのprocess groupを停止し、stoppedの出力を保持します。job数や長時間稼働全体の負荷試験は実施していません。

broker自体の終了/ホスト再起動では保持PTYと出力を失います。この条件はユーザーが許容した範囲です。session別keeperによるbroker再起動越しの復旧は実装していません。[tmuxのkill-server](https://raw.githubusercontent.com/tmux/tmux/master/tmux.1) も全sessionを破棄しますが、それを独自brokerの試験証拠には使いません。

実stdio frontendの終了/別PIDでの再起動、切断中に終了したjobの最終出力/exit 9、counter=1で再実行しないこと、live stdin/resize/stopを確認済みです。詳細は [VALIDATION](../VALIDATION.md)。このソース変更をOCI/ARM64/Tunnel実接続で検証してはいません。
