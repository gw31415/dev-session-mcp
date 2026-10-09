# dev-session-mcp

Linux を複数プロジェクトの開発環境として使う独立した Rust stdio MCP サーバーです。[Shotaro Nakamura 氏の nakasyou/local-mcp](https://github.com/nakasyou/local-mcp) のファイル操作・Codex sandbox を基礎に、独立 broker が保持する実行と MCP Events を追加しています。upstream の公式配布ではありません。[由来と更新方針](docs/UPSTREAM.md)、[著作権表示](NOTICE.md) を参照してください。

公式 Secure MCP Tunnel の認可を入口に使い、stdio frontend が private Unix socket で broker に接続します。ネットワーク上に shell や独自 HTTP/OAuth サーバーを公開しません。既存の Ubuntu ユーザーで利用でき、新しい UID、tmux、Node、Mac、GHA は不要です。導入・安全な更新は [INSTALL](deploy/INSTALL.md)。

## 小さい API

| ツール | 用途 |
| --- | --- |
| open_session, list_sessions, close_session | canonical cwd と permitted roots を持つ project metadata、実行一覧、管理対象の終了 |
| start_execution | 新規開始前に取得した `key_generation` とキーを保存して重複防止。明示した argv を一度開始。profile は必須の host / sandbox / admin:<id>、io は pty / pipes（admin は pipes のみ） |
| input_execution, resize_execution, signal_execution | 明示した execution_id へ stdin、端末サイズ、INT / TERM / KILL |
| read_execution | 通常は `view:summary` で短い状態を取得。必要時にcursor差分detailを非破壊読取（互換用の既定値はdetail） |
| checkpoint_execution | 読者別の詳細処理cursor・目的・完了条件をrevision CASで永続保存。検証済み完了は `completed:true` として履歴縮約を許可。[復旧契約](docs/DURABLE-RECOVERY.md) |
| wait_execution | 指定execution/cursorから出力・状態・終了を上限付きで待つ独自の通常read-only tool。[契約・検証・次の試験](docs/WAIT-EXECUTION.md) |
| read_delivery_diagnostics | 実行状態と通知停止を分離するread-only診断。lease期限・未確認batch・HTTP status/送信時間。詳細と残工程は[配信診断](docs/DELIVERY-DIAGNOSTICS.md) |
| read_file, write_file, list_directory, get_image | 上限付きファイル操作。write は sandbox 内の許可 root のみ |
| import_file | 正式 fileParams の bytes を保存。管理者が確認した HTTPS origin の設定が必要 |

`open_session({"cwd":"/home/ubuntu/projects/example"})` は同じ canonical directory に同じ ID を返します。shell を自動起動しません。`start_execution({"session_id":"…","command":["/bin/bash"],"profile":"host","io":"pty"})` の返す execution_id を以後の操作に使います。新しいIDはspawn前に保存するbroker epoch＋UUIDのopaque handleです。実 PID・Linux process start ticks は記録の別フィールドに保存します。裸のPIDや旧brokerの記録による実行制御は拒否し、保存済み履歴は読み取りで回収します。実行終了後の接続で command を再実行しません。

単一 broker が子プロセス、実行状態、出力 journal を所有します。実行制御の所有者は1つです。selected/current job、暗黙の主 shell、編集 lock、強制 worktree、承認 console はありません。旧 execute/start/run、poll/read snapshots、stop aliases、HTTP/OAuth、legacy initialize は削除しました。MCP 2026-07-28 の `server/discover` と各 request の metadata を使います。

管理者定義profileは `/etc/dev-session-mcp/profiles.json`（broker起動環境の `DEV_SESSION_MCP_PROFILES` で変更可）のargv配列から読み込みます。`profile:"admin:<id>"` は標準Codex sandboxとは独立した生bwrap設定で、安全性は管理者定義です。設定は新規開始ごとに反映し、同じkeyの再送は元の実行・定義hashを返します。[設定・境界・反映方法](docs/SANDBOX-PROFILES-DESIGN.md)、[未検証のサンプル](examples/admin-profiles.json)を確認してください。設定やOS権限を本番へ自動適用しません。

## Events と回収

`server/discover` は `capabilities.events` を宣言し、`events/list` に `execution.events` を返します。`events/subscribe` / `events/unsubscribe` は公式 webhook 形式です。session_id、任意の execution_id で絞り、出力・開始・終了・入力受領を最大 100 ms 待ってまとめて配信します。[配信仕様と限界](docs/EVENTS.md)。

イベントは sequence と cursor を持ちます。重複・順序逆転を受け入れ、sequence で重複を除きます。Webhook の 2xx は受領確認であり、アプリ処理完了を証明しません。`input_execution` はreceipt/eventを **prepared** として永続化してからキュー投入し、保存失敗なら未送信エラー。投入後の返答は **accepted**、OS 書込みは後続の **written / delivery_unknown**。投入後の受付記録保存失敗も成功ACKにせず **delivery_unknown** を返します。`close_stdin:true` は pipes の送信済み text の後で write descriptor を閉じて EOF を送ります。PTY の Ctrl-D は text の `\u0004` で渡す別操作です。write が途中で失敗した可能性や返答喪失がある場合、入力を自動再送しません。無出力だけで入力待ちと断定しません。

journal は全実行で直近 **1 MiB / 1,024 events**、回収 page は **32 KiB**、実行記録と session は各 **64 件**です。満杯なら完了記録を古い順に捨て、live 実行は勝手に停止しません。`catch_up_required` が履歴欠落を示し、`more` なら次の page を回収します。Events は最大 8 subscriptions、outbox は各 1 batch、完全 body は最大 64 KiB。遅い callback が子プロセスの出力 reader を止めません。完全なログは project のファイルへ保存してください。

PTY は stdout/stderr を terminal stream に合流し、ANSI を保持します。pipes は stdout/stderr を分けます。UTF-8 の境界を跨ぐ出力は保持し、無効 bytes は replacement character に変換します。端末画面の emulator ではありません。

## 権限と秘密

**host の command/stdin は、旧 run_command と同じ本人 OS ユーザーの全ファイル・ネットワーク権限を承認なしで使います。** sandbox は session roots への書込みだけを許可し、ネットワークを無効にします。sandbox は pipes のみ。`approved=true` などで拡張できません。read_file/get_image/list_directory は OS ユーザーのファイルアクセスです。

同じ Ubuntu ユーザーを使う本人専用の trusted endpoint です。Tunnel runtime key と Events signing secrets を作業 command の環境へ継承せず、state directory は 0700、socket・秘密 store は 0600、Unix peer UID を検査します。ただし **同一 UID の host command は、読める credential file や Events store、同一 UID process の情報を読めます**。これは新 UID による秘密隔離を保証する設計ではありません。他人や信頼できない agent に公開しないでください。

Tunnel / frontend の再起動を越えて broker と実行・journal を保持します。broker 自体の終了やホスト再起動後、実行の継続は保証しません。新brokerはboundedなexecution/intent/log/checkpointを永続化し、終端未確認は `outcome_unknown` として保持します。[summary/detail・重複防止・容量上限・復旧契約](docs/DURABLE-RECOVERY.md)を参照してください。Events subscription と未確認 batch は disk に保存し、frontend 再起動で再開します。broker再起動後も旧epochの保存済み履歴を参照でき、履歴不足はgap、終端未確認は結果不明として返します。daemon 化して別 process group に離脱した子孫は project 側で管理します。close_session は closing にして独立 signal 経路で終了を要求し、全管理 process の終了確認後に closed を返して metadata を削除します。5秒で確認できなければ closed:false/pending:true と metadata を残し、終了後に同じ close を再試行します。closing 中の新実行・編集は拒否します。終端の closing/exit/closed は既存 journal と実行記録、既存 Events lease で回収・配信できます。closed lease の期限は延長しません。新しい復旧記録は購読のleaseとは独立して保持します。project ファイルは残します。

## Build と確認

```sh
sh scripts/preflight.sh --build
cargo build --locked --manifest-path rust/Cargo.toml
cargo test --locked --manifest-path rust/Cargo.toml
python3 tests/stdio.py rust/target/debug/dev-session-mcp
cargo build --release --locked --manifest-path rust/Cargo.toml
```

Linux、Rust 1.96+、C toolchain / make / perl / pkg-config、bubblewrap、CA certificates が必要です。実 OS/CPU は preflight で検出し、ARM64 も native build します。Codex 依存で初回 build は大きめです。低メモリなら `CARGO_BUILD_JOBS=1`。sandbox が動かない場合は原因を確認し、host に自動降格しません。

[検証結果](VALIDATION.md) はローカル Linux x86_64 の証拠です。この更新の ARM64/OCI 実機、実 OpenAI Events callback、Tunnel 認証、ChatGPT/dot catalog rescan、systemd 配置は未確認です。稼働中ホストを自動更新していません。保存先は [gw31415/dev-session-mcp](https://github.com/gw31415/dev-session-mcp)。

添付入力の client 要件と出力交換の未実装範囲は [FILE-TRANSFER](docs/FILE-TRANSFER.md)、以前の公開監査は [PUBLICATION-AUDIT](docs/PUBLICATION-AUDIT.md)。上流 snapshot を同梱せず、出典付き派生モジュールと通常の Cargo dependency / lockfile を保守します。

公式資料: [MCP 2026-07-28](https://blog.modelcontextprotocol.io/posts/2026-07-28/)、[OpenAI MCP Events](https://developers.openai.com/plugins/build/mcp-events)、[Events proposal](https://github.com/modelcontextprotocol/experimental-ext-triggers-events/blob/main/docs/design-sketch-proposal.md)、[Secure MCP Tunnel](https://developers.openai.com/api/docs/guides/secure-mcp-tunnels)、[tunnel-client configuration](https://github.com/openai/tunnel-client/blob/master/docs/configuration.md)。
