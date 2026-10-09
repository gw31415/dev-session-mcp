# Sandbox設定案とVPS診断（2026-10-09）

状態: **管理者profile実装のローカルレビュー用。本番未適用**。基準main: `9b75422b4e87e7352e9b54d59789226e140106e5`。
診断と開発・テストはhostで実施。実装はローカル変更のみで、本番の独自profileは有効化していない。
本番session/state、サービス、OSポリシー、認証、ネットワーク設定は変更していない。

## 結論と確度

- **今回の再現で証明済み**: `/usr/bin/bwrap --ro-bind / / --unshare-net -- /bin/true` が
  `bwrap: loopback: Failed RTM_NEWADDR: Operation not permitted`、exit 1となる。
  対応するkernel監査ログに、AppArmorの `unconfined` → `unprivileged_userns` 遷移と
  bwrap子プロセスの `net_admin` 拒否がある。ループバックアドレス設定に必要な操作を
  AppArmorが拒否したことが、今回の直接原因である。
- **比較でも証明済み**: 明示的な別プローブで `--unshare-net` を除いても
  `setting up uid map: Permission denied`、exit 1。uid_mapのAppArmor拒否も記録された。
  よってネットワーク許可sandboxを追加するだけでは、このVPSで動作するとは言えない。
- **推定の範囲**: 過去のdev-session実行の同一エラーも同原因と整合する。ただし過去の当該PIDの
  監査ログとの対応は今回確認していない。下記の縮小再現を「過去実行全体のトレース」とは扱わない。
- `profile: "host"` は既にno sandboxを実現している。新しいhost用API/別名は不要。
  明示的にhostを選ぶことと、sandbox失敗後にhostへ自動再実行することは別であり、後者は禁止する。

## 基準mainの標準sandbox実装とargv

参照: `rust/src/base/sandbox.rs`, `rust/src/broker.rs`, `rust/src/server.rs`。
依存Codex commit: `20fedafff83f5c681fc62f73b0ca3227e42e3f8b`（Cargo固定）。
ローカル依存ソースは `.cargo/git/checkouts/codex-9eee5d47a939c68c/20fedaf/codex-rs/`。

`start_execution` のprofileは必須で、`sandbox | host`。profileの暗黙デフォルトは**現状ない**。
`io`省略時はpty、sandboxはpipesのみ。sandboxのcwdはsession permitted roots内が必要。
hostはCommand/PTYを直接起動し、OSユーザーの権限を持つ。両者とも環境変数はsafe_environmentで絞る。
これはファイル読取権限を除去する仕組みではない。
`write_file`は常にsandbox helperを使い、失敗時hostへのフォールバックはない。

Linuxのsandbox呼出しは直接bwrapではなく、現行バイナリ自身のargv[0]を
`codex-linux-sandbox`にして以下の**ソースから導出されるargv形**で呼ぶ。
`<...>`は説明用プレースホルダーであり、そのまま実行するコマンドではない。

```text
argv[0]=codex-linux-sandbox
--sandbox-policy-cwd <canonical-cwd>
--command-cwd <canonical-cwd>
--permission-profile <serialized-workspace-write-permissions>
-- <requested-command...>
```

permissionは `workspace_write_with(roots, Restricted, true, true)` をcwdでmaterializeする。
legacy Landlockとmanaged-network/proxyの引数はどちらもfalse。
依存の `sandboxing/src/landlock.rs` と `linux-sandbox/src/{bwrap,launcher,linux_run_main}.rs` より、
次段は概ね以下。これは実プロセスの全argvを採取した記録ではなく、固定ソースの分岐を示す。

```text
bwrap --new-session --die-with-parent
  <filesystem-mount-args>
  --unshare-user --unshare-pid --unshare-net
  [--proc /proc] [--chdir <normalized-cwd>]
  --argv0 codex-linux-sandbox -- <self-exe> <inner-stage-args...> -- <command...>
```

filesystem argsはread-all時の `--ro-bind / /`、最小devの `--dev /dev`、
許可rootへの `--bind <root> <root>`、保護subpathへのread-only mount等を構築する。
新しいmount namespace、user/pid/net namespaceとinner段のno_new_privs/seccompを組み合わせる。
この設定を「秘密ファイルも読めない完全なコンテナ」と説明してはいけない。
依存にはproc mountのpreflight後に `--proc /proc` を省略する経路がある。
これはhostプロファイルへのfallbackではないが、常にfresh procがあると保証してはいけない。

launcherはPATHのsystem bwrapを `--help` で調べ、`--perms`対応なら優先する。
なければbundled版、いずれもなければエラー。system版はこのVPSでは0.9.0で
`--perms`/`--argv0`対応。上記の縮小プローブは明示的に `/usr/bin/bwrap` を使用した。
本番プロセスのPATH/envを丸ごと読み取っておらず、本番各実行が実際に選んだbwrapパスまでは未採取。

## 診断のコマンドと結果

全て通常ユーザーで実施。sudo、aa-exec、cap追加、sysctl変更、OS制限の迂回は行っていない。
以下は秘密を含まない選択項目だけの記録。PIDは診断時点の値で、将来の操作対象に使わない。

| コマンド/読取 | 結果 |
| --- | --- |
| `git rev-parse HEAD` / `git status --short` | 上記main。診断開始時点で追跡ファイル変更なし |
| `uname -srmo` | Linux 6.17.0-1020-oracle aarch64 GNU/Linux |
| `/usr/bin/bwrap --version` | bubblewrap 0.9.0 |
| `/usr/bin/bwrap --help` の4項目だけ抽出 | `--argv0`, `--perms`, `--unshare-net`, `--share-net`あり |
| `/usr/bin/bwrap`のstat、Python `os.listxattr` | root:root、0755、setuidなし、security.capabilityなし |
| `cat /proc/sys/kernel/unprivileged_userns_clone` | 1 |
| `cat /proc/sys/kernel/apparmor_restrict_unprivileged_userns` | 1 |
| `cat /proc/sys/user/max_user_namespaces` | 47317 |
| `cat /sys/module/apparmor/parameters/enabled` | Y |
| `/etc/apparmor.d/bwrap`存在確認 | 存在せず。これだけでロード済み全profileの不在とは断定しない |
| `cat /etc/apparmor.d/unprivileged_userns` | `audit deny capability`、`audit deny change_profile`、allow userns/network/file等。exec後も同profileを継承する規則 |
| `command -v strace` | 利用不可。インストールしなかった |
| `df -m .` | 初回の読取診断時点で1170 MiB available。この時点ではビルド/依存取得なし |

### 診断時に観測したCodexとservice制限

以下は2026-10-09の診断プロセスとその祖先の観測記録である。
Codex内部の設定値を断定せず、実際に適用されていた制限を記述する。

`/proc/<pid>/status` はUid/Gid、CapInh/Prm/Eff/Bnd/Amb、NoNewPrivs、Seccomp、Seccomp_filters、PPidのみ、
`attr/current`, `cgroup`, `uid_map`, `gid_map`, `ns/{user,net,mnt,pid}`, `exe`を読み取った。
環境変数、認証ファイル、個人DB、プロンプト/通常のcmdline全体は出力していない。
Codex cmdlineはsandbox/approval/permissionsフラグと対応するconfigキーのみallowlist抽出し、
該当引数は空だった（`--sandbox=value`形も確認）。引数が空というだけでは有効設定は決定できない。
個人設定ファイルは読んでいない。観測した診断プロセスには独立したnamespaceやseccomp filterは
見られなかったが、この結果だけで全てのCodex操作や他の実行にも隔離がないとは保証できない。

観測した親子関係:
`diagnostic python 11701 → bash 11700 → Codex 10587 → broker 10317 → frontend 10314 → tunnel 10304`。
Codex executableはmise配下0.154.0、broker/frontendはrelease `9b75422`。

観測した各プロセスはUID/GID1001、effective/permitted/inheritable/ambient capabilitiesは0、
boundingは `000001ffffffffff`、NoNewPrivs=0、Seccomp=0、Seccomp_filters=0、AppArmor=`unconfined`。
全て `system.slice/dev-session-mcp-tunnel.service` 配下。
user/net/mnt/pid namespaceは祖先と一致し、それぞれ
`4026531837 / 4026531833 / 4026531832 / 4026531836`。
uid/gid mapは `0 0 4294967295`。
PID1のnamespaceリンクはPermissionErrorになったため追跡を止め、権限を上げなかった。
したがってPID1との直接比較は未確認。親がunconfinedでも、userns作成後の子は別profileへ遷移し得る。

```sh
systemctl show dev-session-mcp-tunnel.service \
  -p MainPID -p User -p NoNewPrivileges -p PrivateNetwork -p PrivateUsers \
  -p ProtectSystem -p RestrictNamespaces -p SystemCallFilter \
  -p RestrictAddressFamilies -p AppArmorProfile -p AmbientCapabilities \
  -p CapabilityBoundingSet -p KillMode
```

結果: MainPID10304、User ubuntu、NoNewPrivileges/PrivateNetwork/PrivateUsers/ProtectSystem/RestrictNamespaces=no、
SystemCallFilter=`~`、RestrictAddressFamilies=`~`（空の除外集合）、AppArmorProfile/ambient capabilities空、
CapabilityBoundingSetはサポートされる全cap、KillMode=process。
観測プロセスにもseccomp filterはなく、serviceのこの設定から今回のnetlink拒否を説明する証拠はない。
capability bounding setに含まれることは、effective capabilityがあることを意味しない。

### 短命プローブ

Python `subprocess.Popen` で以下の各argvを一度ずつ起動し、`communicate(timeout=5)`で回収した。
環境は `PATH=/usr/bin:/bin`, `LANG=C.UTF-8`だけ。ネットワーク接続やホストinterface変更は行わない。
2番目は事前に明示した比較実験であり、アプリの自動fallbackではない。

```sh
/usr/bin/bwrap --ro-bind / / --unshare-net -- /bin/true
# PID11813、子11814、exit1:
# bwrap: loopback: Failed RTM_NEWADDR: Operation not permitted

/usr/bin/bwrap --ro-bind / / -- /bin/true
# PID11815、子11816、exit1:
# bwrap: setting up uid map: Permission denied

/usr/bin/unshare --user --map-root-user /bin/sh -c \
 'cat /proc/self/attr/current; sed -n "/^CapEff:/p; /^NoNewPrivs:/p; /^Seccomp:/p" /proc/self/status; readlink /proc/self/ns/user /proc/self/ns/net'
# PID11817、exit1、inner shellには到達せず:
# unshare: write failed /proc/self/uid_map: Operation not permitted
```

監査読取（昇格なし）:

```sh
journalctl -k --since '5 minutes ago' --no-pager \
  --grep='apparmor=.*(bwrap|unshare|net_admin|userns)' -n 15 -o cat
```

関連する自分のプローブのイベントだけを要約。時刻は `1791520369`、audit番号170–177。

| audit | PID | 観測内容 |
| --- | --- | --- |
| 170 | 11813 bwrap | userns_create、unconfinedからunprivileged_usernsへ遷移、execpath=/usr/bin/bwrap |
| 171 | 11814 bwrap | unprivileged_usernsでcapability8 setpcap DENIED |
| 172 | 11814 bwrap | 同profileでcapability12 net_admin DENIED |
| 173 | 11815 bwrap | 同じuserns profile遷移 |
| 174 | 11816 bwrap | setpcap DENIED |
| 175 | 11816 bwrap | open DENIED、name=proc/11816/uid_map、error=-13、mask=wr、disconnected path |
| 176 | 11817 unshare | 同じuserns profile遷移 |
| 177 | 11817 unshare | capability21 sys_admin DENIED |

ネットワークを許すAppArmor規則があっても、capability拒否は解消しない。
userns_clone=1も「その中で必要な全操作が許される」の保証ではない。
今回のRTM_NEWADDRはDNS、外部接続、MCP transportの問題ではなく、新netns内のloopback設定時の拒否。
AppArmor以外のあらゆるLSM/ホスト制約の不存在まで証明したものではない。

## 管理者定義profileの設定方法

固定のnetworkフィールド案は撤回した。既存 `start_execution.profile` で
`host`、`sandbox`、`admin:<id>` を選ぶ。toolは増やさず、任意bwrap argvや設定ファイルパスを
MCP入力に追加しない。未知のstart引数はbrokerも拒否する。

| profile | 実行方式と保証 |
| --- | --- |
| `sandbox` | 従来のCodex workspace-write、network Restricted、内側seccomp等を維持。pipesのみ |
| `host` | 従来のno sandbox。pipes/pty。OSユーザーに許された操作が可能 |
| `admin:<id>` | 管理者のargvによる直接bwrap起動。pipesのみ。隔離の内容は設定次第で、標準sandbox相当とは保証しない |

独自設定は標準に継ぎ足さない。生bwrapの内側にCodex helperを置かず、Codex独自seccompも
追加しないので、管理者がnetwork共有を選んだのに内側の別policyが遮断する不整合を避ける。
OS/AppArmorや親プロセスから継承した制限は残る。独自profileには標準の保護subpath処理等も
自動追加されない。必要なmount、namespace、capability処理は管理者が定義する。

### 配置、読込、反映

既定の配置は `/etc/dev-session-mcp/profiles.json`。
別配置は**broker起動環境だけ**の `DEV_SESSION_MCP_PROFILES=/absolute/path/profiles.json` で指定する。
新frontendがbrokerを自動起動するときはこの値を渡す。既存brokerへ接続したfrontendの環境値では
接続先の設定を変更しない。payload用safe_environmentへこの変数を引き継がない。
本番wrapperやサービス設定は今回変更していない。

JSON version 1。全体64 KiB、profile数32、IDは英数字/`_`/`-`の1–64文字、同名重複は禁止。
各定義は `id`, `bwrap`（絶対実行パス）, `args`（文字列argv配列）のみ。
展開後argvは512要素、1要素4096 bytesまで。サーバーは未知field、設定所有者/モード等も検査する。
設定名は公開識別子なので秘密を含めない。

[サンプル設定](../examples/admin-profiles.json)は**未検証の管理者向け提案例**。
namespace/mountとnetwork共有の例であり、標準sandboxの複製ではない。
このVPSで成功することは保証せず、配置するだけでAppArmor問題が解消するものでもない。

```json
{
  "version": 1,
  "profiles": [{
    "id": "workspace-host-network",
    "bwrap": "/usr/bin/bwrap",
    "args": [
      "--new-session", "--die-with-parent",
      "--ro-bind", "/", "/", "--dev", "/dev", "--proc", "/proc",
      "{{writable_roots}}",
      "--unshare-user", "--unshare-pid", "--unshare-ipc", "--unshare-uts",
      "--cap-drop", "ALL", "--chdir", "{{cwd}}"
    ]
  }]
}
```

新規の独自profile起動ごとに全設定を開いて検証し、その一つのsnapshotからargvとhashを作る。
管理者は別ファイルへ完全に書込み、所有者/権限を確認後、同一filesystem内のrenameで原子的に置換する。
途中書込みや一部定義だけ不正なファイルでは独自profileの新規起動を全て拒否する。
ファイル内容変更のためのbroker再起動やreload toolは不要。設定パス変更にはbrokerの計画的再起動が必要。
既定パスが存在しない場合は独自profileなし。明示パスの欠落/読取拒否は設定エラー。
標準sandbox/hostと既存実行のread/replayは設定不良に巻き込まない。

### 所有権と管理者の責任

設定は通常ファイルのみ、最終パスのsymlinkは拒否、rootまたはbroker UID所有、group/other書込禁止。
これは最低限の検査であり、親ディレクトリや実行ファイルの全置換経路を検証する隔離機構ではない。
同UID所有の設定をhost子プロセスが変更できる場合、管理者とpayloadの間に権限境界はない。

設定を境界として扱うには、root等の別責任主体が設定ファイルと全親ディレクトリ、bwrap実体と
その親ディレクトリ、選択元wrapper/service設定を保護し、broker UIDが置換できないことが必要。
root所有のファイルだけをユーザー書込可能なディレクトリに置く構成は不十分。
rootの書込作業をアプリ側から代行する編集toolは作らない。
同UIDのhost実行を許す現製品を、敵対的な同UIDプロセスからの多利用者隔離境界として宣伝しない。

### argv契約

shell文字列eval、環境変数展開、glob、文字列の空白分割は行わない。
実際のargvは `[configured-bwrap, expanded-options..., "--", requested-command...]`。
コマンドはサーバーだけが末尾へ付加する。管理者args内のコマンド位置の裸の語/`--`、
`--help`/`--version`は拒否する。optionのoperandとしての文字列`--`は通常の値として扱う。
`--option=value`形は非対応で、optionとoperandを別要素にする。

| 配列要素の完全一致placeholder | 展開 |
| --- | --- |
| `{{cwd}}` | 検証済み実行cwdを1要素として使用 |
| `{{session_cwd}}` | sessionのcanonical cwdを1要素として使用 |
| `{{writable_roots}}` | session permitted rootごとに `--bind`, root, root を展開。option位置で使用 |

部分文字列への埋込み、未知の`{{...}}`、秘密/環境変数のplaceholderは拒否。
空白等を含むパスも一つのoperandである。cwdは標準同様session roots内に制限するが、
管理者argvが別のhost pathをmountすること自体をこのチェックが禁止するわけではない。

実装はbubblewrap 0.9の一般的な非FDオプションを広く扱う。
namespace、mount/bind/tmpfs/proc/dev、環境、uid/gid、capability、hostname、SELinux label、
chmod/perms/size、argv0、lifecycle等の順序・重複・値は管理者が制御する。
これは安全性を保証する小さなオプションallowlistではなく、コマンド境界を維持するarity文法である。
管理対象PIDは起動したbwrapであり、設定やpayloadが切り離した全子孫の終了までは保証しない。
管理者はdie-with-parentやPID namespace等のlifecycle設定も選択・検証する。
管理者が緩いmountや`--*-try`を使えば保証も弱くなる。互いに矛盾する値や実行bwrapで未対応の値は
bwrapが拒否し、hostへ再実行しない。今後の未認識オプションはparser更新まで明示エラーとする。

FD受渡しは未実装なので、`--args`, `--userns`, `--userns2`, `--pidns`, `--sync-fd`,
`--bind-fd`, `--ro-bind-fd`, `--file`, `--bind-data`, `--ro-bind-data`, `--seccomp`,
`--add-seccomp-fd`, `--block-fd`, `--userns-block-fd`, `--info-fd`, `--json-status-fd`を拒否する。
入力stdinやbrokerのFDを流用しない。従って独自seccomp BPFの注入も今回の設定では非対応。

### 呼出し、再発見、設定変更とidempotency

```json
{"session_id":"<session>","command":["/bin/true"],"profile":"admin:workspace-host-network","io":"pipes"}
```

既存list_sessions/open_sessionへ `execution_profiles` を追加し、対応版、利用可否、ID、
定義のSHA-256、backendと安全性表示だけを公開する。設定全文、argv、設定パスは公開しない。
`available`はカタログを解釈できるという意味（既定ファイルなしも正常な空カタログ）で、
OSで隔離起動が成功する保証ではない。
JSON parse/検証エラーも秘密を含む値やfield名をそのまま返さない。

新規executionは `execution_policy` に以下を永続保存する。
`id`, `definition_sha256`, `backend:"bubblewrap-direct"`,
`security:"administrator-defined"`, `codex_inner_seccomp:false`。
hashはcanonicalな定義（id、実行パス、未展開argv）のSHA-256で、ファイル全体の整形や
無関係profileの変更には依存しない。binary内容、mount元内容のhashや起動成功証明ではない。
実行ファイルを同じパスで更新する場合も別途管理者が版管理・再検証する。
秘密の保管庫ではないので、設定へcredentialそのものを記述しない。
アプリは設定全文をログ/Resourcesへ転記しないが、bwrapのエラーやpayload自身の出力・proc argvで
設定値が見える場合がある。任意プログラムの出力を完全に秘匿する保証はない。

要求fingerprintは従来どおり呼出し内容から作り、既存host/sandboxのkey互換を維持する。
同key/同要求の再送は**設定を再読込する前**に元のexecutionと元のpolicy hashを返す。
設定変更・削除・破損後も再実行しない。別profileを同keyで送れば要求不一致として拒否する。
新しい定義で別の仕事を開始する時だけ、新しいkeyを使用する。結果不明の旧要求へ新keyを振り直さない。
実行中の設定変更は適用しない。list/read/wait summary/detail/Resourcesも保存済みmetadataを返す。
設定ファイルを復元しても既存実行の設定を巻き戻したことにはならない。

### 互換性と本人用プリセット

brokerの既存pingへ `admin_profiles:1` を追加する。新frontendはadmin指定を送信する前に
これを確認し、旧brokerまたは確認不能ならコマンドを送らずエラーにする。
旧frontendはadminを選べないが、host/sandboxはそのまま使える。
新frontendが独自設定を公開しても、接続先の対応確認なしに成功扱いしない。
既存snapshotにはpolicy metadataを捏造せず、世代・cursor・checkpoint契約も変更しない。

製品の標準はsandboxのまま、APIでは引き続きprofile必須。
本人のクライアント側プリセットだけを通常no sandboxとし、毎回 `profile:"host"` を明記する。
networkフィールドはない。batchはpipes、対話用途はptyを選ぶ。

```json
{"session_id":"<session>","command":["/bin/true"],"profile":"host","io":"pipes"}
```

本人がsandbox/adminを選んだ時はその指定を優先し、失敗後hostへ自動変更しない。
プリセット読込失敗時もhostを補完せず標準を明示するか選択を求める。
他ownerのプリセット、server全体の権限、write_fileの標準隔離は変更しない。

移行はローカルレビュー → broker/frontendの対応版更新 → 管理者による設定対象とリスク確認 →
保護された配置・小さな実機検証 → 利用側でID選択、の順。
今回提供するsampleの本番配置やOS設定変更は含まない。
rollbackは新規admin起動を止め、管理者が旧設定を原子的に復元する。既存実行をkillしない。
新旧binaryの切替が必要な場合はactive実行・stateを保全する別の計画として扱う。

network共有はInternetだけの許可ではなくbrokerのnetns共有であり、localhost/LAN/外部通信やlistenを
OSが許す範囲で可能にする。秘密ファイルの非可読性もこの設定からは保証しない。
独自profileはLinuxのみ。macOS/Windowsで同一保証として広告しない。

## AppArmorを対象限定で解決する管理者向け提案

**以下は未検証の設定・運用提案であり、今回適用していない。** 本人用hostプリセットの利用と、
sandboxを正常起動させるOS設定の整備は別の判断である。後者には管理者による対象・互換性の確認が必要。
AppArmor全体の無効化、全体をcomplainにする変更、userns関連sysctlの緩和、共通の
`unprivileged_userns` profileからのdeny削除は既定案にしない。

### 対象とprofileの選定

Ubuntuは、usernsを必要とするbwrapに専用profileを適用する案を示している。
まず当該Ubuntuリリースで保守される適合profileを確認し、なければ管理者が上流の
[bwrap-userns-restrict](https://gitlab.com/apparmor/apparmor/-/blob/master/profiles/apparmor/profiles/extras/bwrap-userns-restrict)
を設計の参考にする。調査時の上流例はABI 5.0を指定し、bwrapの準備処理を許可する一方、
exec先にはprofileをstackしてcapabilityを拒否する構成である。
単に `userns,` や `capability net_admin,` を追加すれば解消すると保証するものではない。
このVPSではuid_map等も拒否されており、exec遷移やmountを含めたpolicy全体の検証が必要である。
[Ubuntuの提案と適用上の注意](https://discourse.ubuntu.com/t/understanding-apparmor-user-namespace-restriction/58007)。

適用対象はargv[0]の `codex-linux-sandbox` という名前や起動用shellではなく、実際にexecされた
bwrapの実体パスと、そのexec連鎖に適用されるprofileで判断する。縮小再現の実体は `/usr/bin/bwrap`。
本番のsandbox起動については実体をまだ採取していないため、管理者は承認された短命testの
`/proc/<pid>/exe`、AppArmor label、auditのexecpathを確認してからattachmentを決める。
既存profileとの重複attachment、親からの制限継承、payloadへのstackも確認する。

`/usr/bin/bwrap`へのprofileはdev-sessionだけでなく、その実体を起動する他アプリ/他ユーザーにも
影響する。これはOS全体の緩和ではないが、「本人だけ」の設定でもないため、利用者を調査して合意する。
本人/サービスだけに限定する必要がある場合は、管理者所有で一般ユーザーが置換できない専用helperと
その正確なattachment/exec遷移を別途設計する。現行のPATH優先選択も含む変更になるため、
単なるbinaryコピーやPATH差替えをこの問題の回避策として実施しない。
broker全体、ユーザーhome全体、任意releaseに一致する広いwildcardへの許可も避ける。

AppArmorのallowはOS capabilityを新規付与するものではない。新profileがbwrapのuserns内の
準備処理を許しても、ホストのambient/file capabilitiesやsetuidを追加する理由にはならない。
profileはファイルパスに基づくため、ファイルと親ディレクトリの所有者・書込権限も確認する。
[UbuntuのAppArmorモデル](https://documentation.ubuntu.com/security/security-features/privilege-restriction/apparmor/)。

### 版・パス変更と適用前の確認

1. OS、kernel、AppArmor parser/package、feature ABI、bwrapの版・実体パス・hashを記録する。
   今回はkernelとbwrap版を確認したが、上流ABI 5.0との互換性は未検証。
   ABIの数字だけを下げて通したり、未対応規則の警告を無視して採用しない。
2. 使用するprofileの出典を固定commit/package版で保存し、既存のprofile本体、local include、
   loaded profile名/モード、enable状態、関連cacheの復元方法を記録する。
   上記master URLは参考であり、その時点の内容を未固定で本番へ取り込まない。
3. 候補を通常の自動読込ディレクトリ外へ置き、構文と対応featureを検査する。
   次は**未実行の提案例**。`<candidate-profile>`は管理者が確定するファイルである。

   ```text
   apparmor_parser --skip-kernel-load --skip-cache <candidate-profile>
   ```

   kernelへのロードとcache利用を避けるオプションであり、構文検査成功は動作保証ではない。
   実機parserの対応オプションも先に確認する。
   [apparmor_parser公式マニュアル](https://manpages.ubuntu.com/manpages/noble/man8/apparmor_parser.8.html)。
4. 承認された保守手順で候補のprofile集合だけをロードする。新規はadd、既存を置換する場合はreplaceを
   使い分け、全profileの再読込は不要とする。既存の同名profileを無確認で上書きしない。
   上流例には複数のprofileがあるため、ファイル名1つだけでなく導入する全profile名を管理する。
5. bwrap更新、system版からbundled版への変更、releaseパス変更、kernel/AppArmor更新ごとに再検証する。
   同じパスでもbinary更新後の動作は再確認する。symlinkやbundled版のfd経由execでは実体と
   attachmentを観測し、`/proc/**/fd/**`等への包括許可で対応しない。
   将来ディストリビューションが同種profileを配布した場合の名前/attachment競合も点検する。

### 合格条件

管理者設定の検証は隔離されたtest環境を先に使い、その後このVPSの承認済み短命testで確認する。
既存Codexや本番executionを検証のために停止せず、tempstateで行う。

- 想定したbwrap attachmentとpayloadのstackを実label/auditで確認する。
  RTM_NEWADDR/uid_map拒否の解消だけでなく、正常終了、書込範囲、user/pid/network namespace、
  inner seccompとno_new_privsを確認する。
- payloadが準備用capabilityを保持せず、通常のホスト権限が増えていないことを確認する。
  AppArmor制限の有無はCapEffだけでは判定せず、labelと対象限定のnegative testも用いる。
- restrictedではネットワーク隔離を維持する。独自profileのhost network共有ではbrokerのnetns共有と
  filesystem制限の維持を確認する。確認先はtest用loopbackのみとし、実credential/外部サービスを使わない。
- 対象外の短命usernsプローブは従来の制限のまま、同じbwrapを使う他アプリも壊れていないことを確認する。
  新規DENIED、unsupported rule、想定外のprofile遷移があれば不合格とし、規則を無差別に追加しない。

### 元に戻す手順

事前に候補を使った実行を把握し、新規test起動を止めて短命testの終了を待つ。
既存の長時間実行に影響する場合は保守時間を調整し、無断でkillしない。
元のprofileがあった場合は、その保存済み定義/includeを復元して対象profileだけreplaceする。
今回新設したprofile集合だけなら、他の利用者がいないことを確認して候補定義を指定したremoveを行い、
新規ファイルの自動読込も取り消す。**未実行の操作例**は
`apparmor_parser --replace <saved-profile>` または `apparmor_parser --remove <candidate-profile>`。
remove時にも定義ファイルが必要なので、先にファイルを消さない。

profileをunloadすると使用中プロセスの制限を弱める可能性があるため、「削除すれば実行中も安全に元通り」
とは扱わない。復元後は新しい短命プロセスでlabelと元の拒否を確認し、loaded profile/起動時の定義/cacheに
候補だけが残っていないことを点検する。共有cache全消去、AppArmorサービス全停止、sysctl変更はしない。
復元結果を記録し、不一致なら本番sandboxの利用開始を見送る。

## ローカル検証と残る実機検証

- unit: 一般argv、operandとコマンド境界、空白を含むroot、placeholder、FD/未知option拒否、
  定義hashと秘密値非転記、重複/不正設定。既存durable/reader/input契約も回帰する。
- stdio: tempstateとfake bwrapで起動配線を確認。fakeはargvを記録し要求commandを直接execするため、
  **sandboxの安全性を証明するtestではない**。設定変更中の実行、同key replay、restart後の旧hash保持、
  新keyの新hash、list/read/wait/Resources/checkpoint後の一致、設定破損・権限・symlink拒否を検証する。
- 実bwrapと標準sandboxは現VPSのAppArmor拒否をnegative結果として記録する。
  payloadが起動していないことを確認し、OSポリシーを変更せず、成功扱いしない。
- 旧broker fixtureはtempstateで起動し、新frontendがadmin開始を送らず拒否することを確認する。
- 管理者設定を適用する将来の環境では、実bwrapのnamespace、capability、write範囲、network共有と
  restrictedの差をtest用loopbackで検証する。今回のfake成功をその代用にしない。
- ビルドは既存専用 `/tmp/dev-session-diagnostics-target`、直列、
  `CARGO_INCREMENTAL=0 RUSTC_WRAPPER=`。空き200MiB未満で自分のbuild/testだけを停止する。
  本番state、認証、サービス、Mycast targetには触れない。

## 検証結果（ローカル変更、2026-10-09）

全て一時stateと短命child。既存release `9b75422` は旧broker fixtureのbinaryとしてのみ使用し、
既存broker socket/stateへは接続していない。

| 検査 | 結果 |
| --- | --- |
| `cargo test --locked --offline --manifest-path rust/Cargo.toml -- --test-threads=1` | 34 passed |
| 実行ファイル検証追加後の `cargo test ... profiles::tests -- --test-threads=1` | 3 passed |
| `cargo build --locked --offline --manifest-path rust/Cargo.toml` | 成功、専用target再利用 |
| `tests/admin_profiles_stdio.py NEW OLD` | 成功。argv、設定変更、旧hash replay、異常設定、未知結果、再起動、旧broker guard、確定失敗launcherのhost非再実行 |
| `tests/recovery_stdio.py NEW` | 成功。400 same-work工程、失敗後repair、ACK喪失、2 reader、Resources、stdin保存失敗、retirement |
| `tests/wait_execution_stdio.py NEW` | 成功。timeout/cancel、cursor/gap、terminal、旧broker互換 |
| `tests/stdio.py NEW` | **未通過**。最初のsandbox `write_file` が `sandboxed file write failed`。後続項目未実施 |
| `tests/io_control.py NEW` | **未通過**。同じ初期sandbox `write_file` で停止。後続項目未実施 |
| 標準sandboxの短命payload | 起動失敗、payload未実行、hostへfallbackなし。既知AppArmor制約のnegative結果 |
| 実 `/usr/bin/bwrap` の独自profile | exit 1。positiveな隔離動作確認とは扱わない |
| fmt / diff whitespace / JSON例 / Python syntax | 成功 |

総合stdio/I/Oのpositive試験は、管理者が正当に整備した隔離利用可能環境で残る。
今回OS制限を回避してテストを緑にする変更はしていない。fake launcherの成功は実bwrapの保証ではない。
未知結果fixtureは当初の正常shutdownではexitが保存されたため、temp brokerだけをcrashするfixtureに修正し、
保存済み実行がoutcome_unknownとして再発見されることを確認した。
実装後のビルド・検証完了時点で空き約1164MiB。ビルド中に200MiB閾値への到達なし。
上記はcommit/push前のローカル検証記録であり、本番反映・設定有効化は含まない。

## 公式一次資料

- [bubblewrap 0.9.0 manual](https://github.com/containers/bubblewrap/blob/v0.9.0/bwrap.xml):
  namespace/mount引数の意味。share-netはunshare-allとの組合せ用途として説明される。
  独自profileのnetwork namespaceは管理者argvに依存する。`--unshare-net`や
  `--unshare-all`による隔離、`--unshare-all --share-net`による共有を管理者が選ぶ。
  サーバーはこれらを自動追加・削除しない。
- [bubblewrap 0.9.0 network.c](https://github.com/containers/bubblewrap/blob/v0.9.0/network.c):
  loopback_setupがNETLINK_ROUTEで127.0.0.1のRTM_NEWADDR、その後RTM_NEWLINKを行う。
- [bubblewrap 0.9.0 README](https://github.com/containers/bubblewrap/blob/v0.9.0/README.md):
  bubblewrapは低水準機構であり、実際の境界は呼出側の引数/policyに依存する。
- [Ubuntu: Understanding AppArmor user namespace restriction](https://discourse.ubuntu.com/t/understanding-apparmor-user-namespace-restriction/58007):
  unprivileged user namespaceとAppArmor制限の関係。一般解説と当該VPSの監査証拠を区別した。
- [固定Codex bubblewrap引数生成](https://github.com/openai/codex/blob/20fedafff83f5c681fc62f73b0ca3227e42e3f8b/codex-rs/linux-sandbox/src/bwrap.rs)
  および同commitの `launcher.rs`, `linux_run_main.rs`, `sandboxing/src/landlock.rs`。
  今回これらのコードはローカルの固定checkoutで確認した。
