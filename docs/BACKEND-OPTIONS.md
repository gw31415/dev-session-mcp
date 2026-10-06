# Persistent terminal backend options — investigation

2026-10-06 UTC。現実装はtmuxを保持し、callerの通常APIはsession_idによるrun_command/read_output/send_stdin/stop_commandへ整理済みです。依存置換は未実施です。

候補は [shpool/libshpool](https://github.com/shell-pool/shpool)。named shellの永続化・再attachに特化し、画面分割/レイアウトはありません。公開 [libshpool API](https://docs.rs/libshpool/latest/libshpool/) はArgs/Commands/run/Hooks中心です。[runの安全条件](https://docs.rs/libshpool/latest/libshpool/fn.run.html) により、MCPの多thread runtime内へ直接混在させず、同一binaryの専用サブコマンドを別processで保持する案が適切と判断します。

[shpool-protocol](https://docs.rs/shpool-protocol/latest/shpool_protocol/) にはattach、入力/出力stream、resize、list/kill向けの型があります。通常toolからこのprivate Unix socketへadapterを付ける最小案です。1 sessionにつきactive attachmentが1つという制約、切断中にcommandが終了した場合の最終output/exit code保持、bounded履歴の取得は採用前に実確認が必要です。libraryのrunだけでMCPのjob管理APIが完成するわけではありません。

[portable-pty](https://docs.rs/portable-pty/latest/portable_pty/) はPTYの生成、子process、入力/出力、resizeの基盤です。独立broker、session/job対応、再接続、bounded output、終了結果の保存、socket認証/permissionをこちらで実装する必要があります。完成した永続session poolとして紹介しません。

必須のstdio/Tunnel frontend切断・再起動越しの継続は、PTY所有processを独立して生かす設計で満たせます。ただしPTY所有daemon自体が停止すると、上の最小案では再attachできるPTYを失います。brokerの再起動も越える必要があれば、session別keeperがPTYを保持し、再起動するbrokerは再発見/ルーティングだけを担当する追加設計が必要です。これも未実装です。

従来の実testが確認したのはMCP stdio processの終了/再起動です。tmux server自体の再起動は確認していません。[tmux公式manual](https://raw.githubusercontent.com/tmux/tmux/master/tmux.1) のkill-serverも全sessionを破棄します。この条件をdaemon再起動耐性の証拠として扱いません。

比較の結論: 既製の永続shell poolを使うならlibshpoolが有力候補です。MCP向けのjob結果保持やbroker再起動耐性まで独自に揃えるならportable-pty + session keeperは制御しやすい一方、独自実装量が増えます。選定と実backendの疎通は未完了で、現在の機能・依存は削除していません。
