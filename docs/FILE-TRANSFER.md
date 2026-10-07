# File exchange: attachment import and output boundary

[nakasyou/local-mcp](https://github.com/nakasyou/local-mcp) 由来のread_file/write_file等に対し、ここに記載するimport_fileとURL取得制限はdev-session-mcpの追加実装です。upstreamへの実装済み機能として扱いません。[由来と保守範囲](UPSTREAM.md)、[NOTICE](../NOTICE.md)。

正式な入力仕様は [Plugins File APIs](https://developers.openai.com/plugins/reference#file-apis) です。ChatGPTがtool descriptorのopenai/fileParams指定に応じて一時download URL/file IDを渡します。使用中のrmcp 3.5.1はtool metadataを保持できます。import_fileを追加し、ファイルbytesを会話本文へ載せずHTTP bodyから作業ファイルへ直接保存します。

## import_file

fileというtop-level fieldへopenai/fileParamsを指定しています。file schemaはdownload_url/file_idを必須とし、mime_type/file_nameもoptionalで宣言します。ユーザーの正式添付からclientが供給するfile objectを使い、AIにURLやfile IDを作らせません。session_idと保存先pathを指定し、返却はpath、bytes、SHA256だけです。file_nameを保存パスへ流用せず、添付の自動展開・実行もしません。

保存先の親ディレクトリは既存session cwd内に限定します。デフォルトでは既存ファイルを変更せず、overwrite=trueの場合のみ置換します。同じ親ディレクトリ内の0600一時ファイルへstreamし、byte countと任意のexpected_sha256を照合してから配置します。no-overwriteはhard linkによる原子的な作成、明示overwriteはrenameです。失敗/cancelで一時ファイルを破棄し、resumeは提供しません。processの強制終了で残ったpartを自動再利用しません。

128 MiB上限、全体120秒、接続10秒、read idle20秒です。Content-Lengthと実streamの双方を制限し、圧縮body、redirect、200以外を拒否します。proxy環境を継承せず、TLS検証を維持します。秘密のURL/署名やbytesを結果・エラーへ返しません。

## URL取得の信頼範囲

DEV_SESSION_MCP_FILE_ORIGINSは管理者が固定wrapperに設定する完全一致のHTTPS originリストです。既定は空で、取得は拒否します。実clientが渡した配信originを確認してから設定します。公式入力仕様は配信hostの固定値を示していないため、推測のドメインやwildcardを既定許可しません。設定は認証credentialではなく取得先の制限で、作業shell/brokerへ継承しません。

HTTPS/443、DNS hostname、userinfo/fragmentなしを要求します。解決した全IPを検査し、loopback/private/link-local/共有/Tailscaleアドレス/metadata endpoint/特別用途の範囲を拒否します。検査したIPをHTTP clientへ固定し、DNS再解決によるrebindingを避けます。[IANA IPv4](https://www.iana.org/assignments/iana-ipv4-special-registry)、[IPv6](https://www.iana.org/assignments/iana-ipv6-special-registry) を参照した保守的な範囲です。

fileParamsやfile_id自体を暗号学的な認可証明とは扱いません。Tunnelの呼出認可と、管理者が選んだ配信originの制限に依存します。一般のユーザーURLを無制限fetchする機能ではありません。

## 検証範囲と残る出力経路

実MCPでdescriptor metadata/schemaと未設定originの拒否を検証します。ローカルHTTP fixtureでbinary stream/size/hash/失敗時破棄/既存ファイル保全を検証し、外部HTTPSやChatGPTからの実添付配送とは区別します。この実行環境では直接のpublic DNS解決が失敗しており、外部HTTPS取得・実dot/ChatGPT添付入力は未確認です。署名URLの期限が切れたら正式clientから新しいfile objectを取得し、途中bytesを再利用せず初めから転送します。

MCP blob/resourceを自動でLibraryへ保存する公開契約は未確認です。出力連携は未実装で、独自APIや公開HTTP/file shareを実装しません。既存SSH/SFTPを利用できる端末なら、現在もその経路で成果物を受け取れます。dotだけで完結する出力連携には、正式なclient側のupload/download/Library経路の確認と実ファイルの往復試験が残ります。VPSへLibrary認証情報を渡しません。

[MCP resources](https://modelcontextprotocol.io/specification/2026-07-28/server/resources) のfile URIやbinary blobだけではdotのdownload UIを保証できません。巨大Base64をtool本文へ載せたり、file URI/localhost URLを届くdownload URLとして返す方法は採用しません。新しい実credential/grant、network公開、OCI deployは今回行っていません。
