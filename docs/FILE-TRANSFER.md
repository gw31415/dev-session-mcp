# File exchange: attachment import and output boundary

[nakasyou/local-mcp](https://github.com/nakasyou/local-mcp) 由来のread_file/write_file等に対し、ここに記載するimport_fileとURL取得制限はdev-session-mcpの追加実装です。upstreamへの実装済み機能として扱いません。[由来と保守範囲](UPSTREAM.md)、[NOTICE](../NOTICE.md)。

正式な入力仕様は [Plugins File APIs](https://developers.openai.com/plugins/reference#file-apis) です。ChatGPTなどfileParams対応クライアントはtool descriptorの`openai/fileParams`指定に応じて一時download URL/file IDを渡します（非対応クライアントはこのmetadataを無視します）。使用中のrmcp 3.5.1はtool metadataを保持できます。import_fileを追加し、ファイルbytesを会話本文へ載せずHTTP bodyから作業ファイルへ直接保存します。

## import_file

fileというtop-level fieldへopenai/fileParamsを指定しています。file schemaはdownload_url/file_idを必須とし、mime_type/file_nameもoptionalで宣言します。ユーザーの正式添付からclientが供給するfile objectを使い、AIにURLやfile IDを作らせません。session_idと保存先pathを指定し、返却はpath、bytes、SHA256だけです。file_nameを保存パスへ流用せず、添付の自動展開・実行もしません。

保存先の親ディレクトリは既存session cwd内に限定します。デフォルトでは既存ファイルを変更せず、overwrite=trueの場合のみ置換します。同じ親ディレクトリ内の0600一時ファイルへstreamし、byte countと任意のexpected_sha256を照合してから配置します。no-overwriteはhard linkによる原子的な作成、明示overwriteはrenameです。失敗/cancelで一時ファイルを破棄し、resumeは提供しません。processの強制終了で残ったpartを自動再利用しません。

128 MiB上限、全体120秒、接続10秒、read idle20秒です。Content-Lengthと実streamの双方を制限し、圧縮body、redirect、200以外を拒否します。proxy環境を継承せず、TLS検証を維持します。秘密のURL/署名やbytesを結果・エラーへ返しません。

## create_upload

claude.aiは添付ファイルのdownload URL/file IDをMCPツールへ渡しません。会話側のコード実行サンドボックスには添付の実ファイルがありますが、モデルがbase64をツール引数へ書き写す方法は数百KBで実用になりません（約19万文字を誤りなく写す必要があり、失敗しやすい）。そこで向きを逆にし、ホストが受け口を用意してサンドボックスから送らせます。

1. `create_upload(session_id, path, overwrite?, expected_sha256?)` が、保存先1つに束縛した使い捨てURL `<upload-base-url>/upload/<token>` を返す。tokenは乱数244bit、有効15分、1回限り（受信開始時に消費し、失敗したら発行し直す）。同時に保留できるのは64件まで。
2. サンドボックスで `curl -fsS -T FILE URL`（HTTP PUT）。応答は201とpath/bytes/sha256のJSON。

保存先の検査、0600一時ファイル、128 MiB上限、SHA256照合、no-overwriteのhard link/明示overwriteのrename、失敗時の破棄は`import_file`と同じ実装です。受信全体は600秒で打ち切ります。tokenはSHA256で保持し、ログ・エラーへ返しません。

有効化はHTTP frontendの`--upload-base-url https://<公開ホスト>`だけです。未指定ならツールも経路も存在しません（stdio frontendには常に無い）。`/upload/`は`/mcp`の前段認証（Access OAuth）を通れないクライアント（サンドボックスのcurl）から呼ばれるため、そのパスだけ認証プロキシをbypassし、tokenを唯一の認可とします。tokenは認証済みMCP呼び出しでしか発行されません。サンドボックスからホストへの通信には、claude.ai側でそのドメインへのネットワーク許可が必要です（[INSTALL](../deploy/INSTALL.md#claudeai-attachments-create_upload)）。

## URL取得の信頼範囲

DEV_SESSION_MCP_FILE_ORIGINSは管理者が固定wrapperに設定する完全一致のHTTPS originリストです。既定は空で、取得は拒否します。実clientが渡した配信originを確認してから設定します。公式入力仕様は配信hostの固定値を示していないため、推測のドメインやwildcardを既定許可しません。設定は認証credentialではなく取得先の制限で、作業shell/brokerへ継承しません。

HTTPS/443、DNS hostname、userinfo/fragmentなしを要求します。解決した全IPを検査し、loopback/private/link-local/共有/Tailscaleアドレス/metadata endpoint/特別用途の範囲を拒否します。検査したIPをHTTP clientへ固定し、DNS再解決によるrebindingを避けます。[IANA IPv4](https://www.iana.org/assignments/iana-ipv4-special-registry)、[IPv6](https://www.iana.org/assignments/iana-ipv6-special-registry) を参照した保守的な範囲です。

fileParamsやfile_id自体を暗号学的な認可証明とは扱いません。前段プロキシ（Tunnel / Access）の呼出認可と、管理者が選んだ配信originの制限に依存します。一般のユーザーURLを無制限fetchする機能ではありません。

## 検証範囲と残る出力経路

実MCPでdescriptor metadata/schemaと未設定originの拒否を検証します。ローカルHTTP fixtureでbinary stream/size/hash/失敗時破棄/既存ファイル保全を検証し、外部HTTPSやChatGPTからの実添付配送とは区別します。この実行環境では直接のpublic DNS解決が失敗しており、外部HTTPS取得・実dot/ChatGPT添付入力は未確認です。署名URLの期限が切れたら正式clientから新しいfile objectを取得し、途中bytesを再利用せず初めから転送します。

MCP blob/resourceを自動でLibraryへ保存する公開契約は未確認です。出力連携は未実装で、独自APIや公開HTTP/file shareを実装しません。既存SSH/SFTPを利用できる端末なら、現在もその経路で成果物を受け取れます。dotだけで完結する出力連携には、正式なclient側のupload/download/Library経路の確認と実ファイルの往復試験が残ります。VPSへLibrary認証情報を渡しません。

[MCP resources](https://modelcontextprotocol.io/specification/2026-07-28/server/resources) のfile URIやbinary blobだけではdotのdownload UIを保証できません。巨大Base64をtool本文へ載せたり、file URI/localhost URLを届くdownload URLとして返す方法は採用しません。新しい実credential/grant、network公開、OCI deployは今回行っていません。
