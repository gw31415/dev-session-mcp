---
name: dev-session-cli
description: >-
  Run CLI tools on the user's own Linux host through the dev-session-mcp connector when working in an
  online conversation (claude.ai web/mobile/Desktop chat, ChatGPT, etc.) instead of a local terminal.
  Use whenever another skill or the user's request expects local shell access — running a CLI or a
  skill's bundled scripts, reading/writing local files, viewing generated images — and dev-session-mcp
  tools (open_session, start_execution, wait_execution, get_image, ...) are available. The host behind
  dev-session-mcp stands in for "the local machine"; do not install the CLI into the chat's own
  code-execution sandbox. Also use for 「VPS で実行して」「dev-session-mcp 経由で」.
---

# オンラインの会話で、ローカル前提の CLI を dev-session-mcp で代替する

他のスキル（以下「対象スキル」）や依頼が「手元のシェルで CLI を実行する」前提で書かれていても、
オンラインの会話（claude.ai、モバイル、Desktop のチャット、ChatGPT など）では手元のシェルが無い。
dev-session-mcp が接続されていれば、**その先のホストを「ローカル」とみなして**、対象スキルの手順を
そのまま進める。対象スキル自体は書き換えず、操作だけを下の対応表で読み替える。

## 0. 判断

- dev-session-mcp のツール（`open_session`, `start_execution` …）が使えるなら、CLI の実行・ファイル操作は
  すべてホスト側で行う。会話側のコード実行サンドボックスに CLI を導入しない（ネットワーク制限があり、
  会話ごとに消える）。サンドボックスは、ホストへ送る小さなファイルの加工など補助にだけ使う。
- 対象スキルに「リモート」「シェル系 MCP」向けの手順（例 `references/remote.md`）があれば、それを優先し、
  このスキルは足りない部分の補足として使う。
- dev-session-mcp が無ければこのスキルは使わない。対象スキルの「CLI 無し」の代替手順に従うか、
  コネクタの設定を案内する（[README](../../README.md) の「claude.ai などで使う」）。

## 1. 対応表

| 対象スキルの操作 | dev-session-mcp での代替 |
| --- | --- |
| 作業ディレクトリで作業する | `["mkdir","-p",DIR]` → `open_session(cwd=DIR)`。以後の実行・相対パスはその中 |
| シェルコマンドを実行 | `start_execution(command=[...], profile, io="pipes")` → `wait_execution` で出力を受け取る |
| パイプ・リダイレクト・`&&`・グロブ | argv では展開されないので `["bash","-lc","…"]` で包む |
| 対話的な入力 | `input_execution`（EOF は `close_stdin=true`）。PTY が必要なものは `io="pty"` |
| 長い処理 | 同上。`wait_execution` を `max_wait_ms=10000` で繰り返す（タイムアウトは異常ではない） |
| ファイルを読む | `read_file`（64 KiB まで）。大きいものは `head` / `sed -n` / `grep` / `split` の出力で読む |
| ファイルを書く | `write_file`（64 KiB まで、session roots 内のみ）。失敗したら引数渡し（手順 4） |
| 画像を見る | `get_image`（PNG/JPEG/GIF/WebP、1 MiB まで）。大きければホストで縮小・切り出してから |
| ディレクトリを見る | `list_directory` または `["ls","-la",DIR]` |
| 対象スキル同梱のスクリプト（`$SKILL/scripts/...`） | ホストに置いてから実行する（手順 3） |
| ユーザーの添付ファイルを入力にする | 手順 5 |
| 成果物を渡す | 手順 6 |

## 2. 実行の基本

```text
open_session(cwd="/home/<user>/work/<task>")            → session_id, key_generation
start_execution(session_id, command=["bash","-lc","tool --version"], profile="host", io="pipes")
                                                        → execution_id, cursor
wait_execution(execution_id, cursor, max_wait_ms=10000, view="summary")
  … 応答トップレベルの cursor を次の呼び出しへ渡し、終了（exit）まで繰り返す
```

- **PATH**: 実行は環境変数を消した状態で始まり、`~/.cargo/bin` や `~/.local/bin` などが PATH に無いことが多い。
  `bash -lc`（ログインシェルで `~/.profile` を読む）で包むか、`$HOME/.cargo/bin/tool` のように絶対パスで呼ぶ。
  `command -v tool` が失敗しても、未導入と決める前に `ls ~/.cargo/bin ~/.local/bin /usr/local/bin` も見る。
- **profile**:
  - `sandbox` — 書き込みは session roots 内だけ、ネットワーク無し。ネットワークも作業ディレクトリ外への
    書き込みも要らない処理（変換・解析など）はこちらが安全。
  - `host` — 本人の OS ユーザーの全権限・ネットワークあり。ダウンロード・導入・ディレクトリ外への書き込みに使う。
  - `sandbox` がホストの bubblewrap 設定や mise などのシム（読み取り専用環境で失敗する）で動かないときは `host` に切り替える。
  - `list_sessions` の `execution_profiles` に管理者定義の `admin:<id>` があれば、用途に合うものを使ってよい。
- **再送しない**: `start_execution` / `input_execution` の結果が不明（タイムアウト・切断）なら、同じ要求を
  送り直さない。`list_sessions` / `read_execution` で実行が始まっていたか確かめる。再試行の可能性がある
  開始には `idempotency_key`（`open_session` の `key_generation` と組で）を付けておく。
- **出力**: journal は直近 1 MiB、1 ページ 32 KiB。`more` は続きあり、`catch_up_required` は欠落あり。
  大量の出力はファイルに書かせ（`> out.txt`）、必要な部分だけ読む。
- **権限の重い操作は確認する**: `host` は承認なしで本人の全権限を使う。パッケージの導入（`sudo apt`、
  `cargo install` など）、作業ディレクトリ外の削除・上書き、常駐プロセスの起動は、実行前にユーザーに確認する。

## 3. CLI と対象スキルのスクリプトをホストに用意する

1. **CLI**: `["bash","-lc","command -v TOOL && TOOL --version"]` で確かめる。無ければ対象スキルの導入手順
   （`references/install.md` など）を、**ユーザーの了承を得てから**ホストで実行する。
2. **同梱スクリプト**: 対象スキルの `scripts/` はチャット側にしか無い。どれかでホストに置き、
   以後 `$SKILL` をホスト上のパスに読み替える（MCP 経由では環境変数が残らないので、毎回絶対パスで書く）。
   - 対象スキルが Git リポジトリで配布されている（README やスキル本文にリポジトリ URL がある）なら、
     `git clone --depth 1 URL ~/.local/share/agent-skills/<repo>`（`host`）。既にあれば `git -C … pull`。
     スキルの場所はその中の `skills/<name>/` など。
   - それ以外は、スクリプト本文（テキスト、64 KiB 以下）を `write_file` で
     `~/.local/share/agent-skills/<skill-name>/scripts/` 相当の場所（session roots 内）へ書く。
     `write_file` は session roots の外に書けないので、そのディレクトリで `open_session` してから書くか、
     作業ディレクトリ内の `scripts/` に置く。
   - 対象スキルとホストのスクリプトの版がずれないよう、既存のコピーを使うときは更新してから使う。

## 4. テキストをホストへ書く

- 基本は `write_file(session_id, path, content)`（UTF-8、64 KiB 以下）。
- `write_file` が使えない・失敗するとき（サンドボックスが使えないホストなど）は引数で渡す:
  `["bash","-c","printf '%s' \"$1\" > fix.txt","_","<内容>"]`（1 引数 64 KiB まで）。
  または `start_execution(["bash","-c","cat > fix.txt"])` に `input_execution(text, close_stdin=true)`。
- 64 KiB を超えるものは分割して `part.000`, `part.001`, … に書き、`cat part.* > file` で連結する。

## 5. ユーザーの添付ファイルをホストへ渡す

会話に添付されたファイルは、モデルが直接「見る」ことはできても、バイト列のままホストへ送る経路が限られる。
上から順に試す。

1. **`import_file`**: クライアントがダウンロード URL / file ID を渡せる場合（ChatGPT など `openai/fileParams`
   対応）。許可 origin の設定が必要（[FILE-TRANSFER](../../docs/FILE-TRANSFER.md)）。claude.ai は非対応。
2. **`create_upload`**（claude.ai など、添付が会話側サンドボックスにある場合）: ツール一覧に `create_upload` があれば、
   これを使う。バイト列はモデルを通らないので、サイズを気にしなくてよい（128 MiB まで）。
   1. サンドボックスで添付のパス（claude.ai では通常 `/mnt/user-data/uploads/` 以下）を確かめ、`sha256sum` を取る。
      PDF から必要なページだけ画像化するなど、加工はサンドボックスで先に済ませてよい。
   2. `create_upload(session_id, path="input/score.pdf", expected_sha256=…)`（保存先の親ディレクトリは先に作る）。
   3. サンドボックスで `curl -fsS -T /mnt/user-data/uploads/score.pdf '<upload_url>'`。201 と sha256 が返れば完了。
   - URL は 1 回限り・15 分で失効。失敗したら `create_upload` からやり直す。
   - curl が接続できない（プロキシの 403、名前解決の失敗など）なら、claude.ai のコード実行でこのホストへの
     ネットワークが許可されていない。ユーザーに「設定 → 機能 → コード実行」でホストのドメインを許可してもらうか、
     3 以降の方法に切り替える。
   - `create_upload` がツール一覧に無ければ、サーバーが `--upload-base-url` 無しで動いている
     （[INSTALL](../../deploy/INSTALL.md#claudeai-attachments-create_upload)）。管理者に有効化を頼むか、3 以降へ。
3. **既にホストにある**: ユーザーにホスト上のパスを聞き、`cp` / `ln -s` する。同期フォルダや `scp` で
   置いてもらうのが最も確実。
4. **URL がある**: `["curl","-fsSLo",NAME,URL]`（`host`）。
5. **最後の手段（数十 KB までの小さいファイルのみ）**: 会話側サンドボックスで `base64 -w0` し、断片ごとに
   `write_file` で書いて `cat f.b64.* | base64 -d > f && rm f.b64.*`。バイト列がすべてモデルの出力を通るので
   遅く、大量のトークンを使い、1 文字の写し間違いで壊れる。それより大きければ 2〜4 を使う。

添付画像を「見る」こと自体は添付のままでよい。CLI の入力としてホスト上のファイルが必要なときだけ転送する。

## 6. 成果物を渡す

- まずホスト上のパスを伝える（ユーザーはそこから取れることが多い）。
- 本文が必要なら `read_file`（64 KiB 以下）。大きいものは `split -b 60000 FILE out.` で分けて順に読み、連結する。
- 会話側でファイルとして渡せる環境（claude.ai のファイル作成など）なら、読み取った内容をサンドボックスで
  ファイルに書き出して添付する。
- 画像は `get_image` で見せられる（1 MiB 以下）。

## 7. 後片付け

- 一時ファイル（`*.b64.*`、`part.*`）は消す。作業ディレクトリと成果物は残す。
- 常駐させた実行が不要になったら `signal_execution`（TERM → 必要なら KILL）で止める。
