# dev-session-mcp

Linux ホストを複数プロジェクトの開発環境として MCP クライアントから操作する Rust 製 MCP サーバーです。Claude（Claude Code / claude.ai / Desktop）、Codex / ChatGPT、その他の標準 MCP クライアントから、同時に同じホストへ接続できます。[Shotaro Nakamura 氏の nakasyou/local-mcp](https://github.com/nakasyou/local-mcp) のファイル操作・Codex sandbox を基礎にしています（upstream の公式配布ではありません。[由来](docs/UPSTREAM.md)、[著作権表示](NOTICE.md)）。

## 接続方式

| 入口 | コマンド | 主な用途 |
| --- | --- | --- |
| stdio | `dev-session-mcp stdio` | Claude Code などのローカル/SSH 接続、OpenAI Secure MCP Tunnel（tunnel-client） |
| Streamable HTTP | `dev-session-mcp http --listen 127.0.0.1:8808` | claude.ai / Claude Desktop / モバイルのカスタムコネクタ、ChatGPT などリモート接続。`/mcp` で提供 |

- **プロトコル**: MCP 2026-07-28（`server/discover`、リクエスト毎の `_meta`、HTTP はステートレス）と 2025-11-25 / 2025-06-18 / 2025-03-26（`initialize`）の両方に対応します。拡張は [MCP Events](docs/EVENTS.md)（`execution.events`、webhook 配信）を `server/discover` の `capabilities.events` で宣言します。
- **同時接続**: frontend（stdio / HTTP）は状態を持ちません。プロセス・出力 journal・Events 購読はすべて単一の broker が所有し、frontend は private Unix socket で接続します。stdio を何本起動しても、HTTP と併用しても同じセッション・実行が見えます。
- **認証**: このソフトウェア自体は認証を実装しません。HTTP は既定で loopback のみに bind し、Host / Origin ヘッダを検査します。外部公開は Cloudflare Tunnel + Access（OAuth）などの認証付きプロキシに任せてください。手順は [INSTALL](deploy/INSTALL.md)。

## ツール

| ツール | 用途 |
| --- | --- |
| open_session, list_sessions, close_session | project directory のセッション、実行一覧、管理対象の終了 |
| start_execution | argv を一度だけ開始。profile は `host` / `sandbox` / `admin:<id>`、io は `pty` / `pipes` |
| input_execution, resize_execution, signal_execution | stdin（pipes は EOF も）、端末サイズ、INT / TERM / KILL |
| read_execution, wait_execution | 状態（`view:summary`）や cursor 以降の出力。wait は最大 10 秒待機 |
| checkpoint_execution | 読者別 cursor・作業目的を CAS で永続化（[復旧契約](docs/DURABLE-RECOVERY.md)） |
| read_file, write_file, list_directory, get_image | 上限付きファイル操作。write は sandbox 内・session roots のみ |
| import_file | クライアントが渡した HTTPS ファイルを保存（許可 origin の設定が必要、[FILE-TRANSFER](docs/FILE-TRANSFER.md)） |

典型的な流れ: `open_session({"cwd":"/home/ubuntu/projects/app"})` → `start_execution({"session_id":…,"command":["cargo","test"],"profile":"host","io":"pipes"})` → 返った `cursor` で `wait_execution` を繰り返し、応答トップレベルの `cursor` を次に渡す。

- 実行 ID は broker epoch + UUID。裸の PID や別 epoch の ID による制御は拒否します。
- journal は全体で直近 1 MiB / 1,024 events、ページは 32 KiB。`catch_up_required` は欠落、`more` は続きがあることを示します。
- 開始・入力の結果が不明な場合は自動再送せず、`idempotency_key` / execution_id で `list_sessions` から回収します。
- 管理者定義 profile（`admin:<id>`）は `/etc/dev-session-mcp/profiles.json`（`DEV_SESSION_MCP_PROFILES` で変更可）から読み込む生 bubblewrap 設定です。[設計](docs/SANDBOX-PROFILES-DESIGN.md)、[例](examples/admin-profiles.json)。

## 権限と秘密

**`host` は本人の OS ユーザーの全ファイル・ネットワーク権限を承認なしで使います。** `sandbox` は session roots への書込みだけを許可し、ネットワークを遮断します（pipes のみ、host へ自動降格しません）。read_file / list_directory / get_image は OS ユーザーの権限で読みます。

本人専用の trusted endpoint です。broker と作業コマンドは環境変数を消した状態で起動され、クライアントやトンネルの資格情報（`MCP_*`、`TUNNEL_*` など）を継承しません。state directory は 0700、socket・秘密 store は 0600、Unix peer UID を検査します。ただし同一 UID の host コマンドは同 UID の credential file や Events store を読めます。他人や信頼できない agent に公開しないでください。

frontend やトンネルの再起動を越えて broker・実行・journal は保持されます。broker 自体やホストの再起動後は実行の継続を保証しません（記録は保持し、終端未確認は `outcome_unknown`）。HTTP frontend は broker が停止していれば次のリクエストで起動し直します。

## Build と確認

```sh
sh scripts/preflight.sh --build
cargo build --release --locked --manifest-path rust/Cargo.toml
cargo test --locked --manifest-path rust/Cargo.toml
python3 tests/e2e.py rust/target/release/dev-session-mcp
```

Linux、Rust 1.96+、C toolchain / make / perl / pkg-config、bubblewrap、CA certificates が必要です。ARM64 / x86_64 とも native build。Codex 依存で初回 build は大きめなので、低メモリなら `CARGO_BUILD_JOBS=1`。`tests/e2e.py` は実 binary を使い、stdio（2 方式）と HTTP を同時に接続して確認します。[検証記録](VALIDATION.md)。

公式資料: [MCP 2026-07-28](https://modelcontextprotocol.io/specification/2026-07-28/changelog)、[MCP Events proposal](https://github.com/modelcontextprotocol/experimental-ext-triggers-events/blob/main/docs/design-sketch-proposal.md)、[OpenAI MCP Events](https://developers.openai.com/plugins/build/mcp-events)、[Secure MCP Tunnel](https://developers.openai.com/api/docs/guides/secure-mcp-tunnels)、[Cloudflare: Secure MCP servers](https://developers.cloudflare.com/cloudflare-one/access-controls/ai-controls/secure-mcp-servers/)。
