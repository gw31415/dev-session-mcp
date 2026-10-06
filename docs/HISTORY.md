# Source history and checkpoint preservation

旧Node実装を含む既存mainの復元元は `5c2749d847bd853d2767d170eb9fb24b27d0b9ad` です。Node source、package metadata/lockfile、旧service/wrapper、当時の手順とログをGit履歴に保持します。旧版の内容は例えば次のように確認できます。

```sh
git show 5c2749d847bd853d2767d170eb9fb24b27d0b9ad:src/server.mjs
git show 5c2749d847bd853d2767d170eb9fb24b27d0b9ad:docs/LEGACY-TUNNEL-INSTALL.md
```

ローカルcheckpoint `866b626197bbdbe9d9e92d55f37d46f6cdee7c85` はRust検証の追加、`a72421f41733d7b7e887a090b722135a691b7aad` はRust stdio + Tunnel手順の完成版です。ソースtreeは順に `ebb773c3ca0f9e2dd0ac571f6e2301b5032d4c6f` と `6c1b0c6346db3ead2c4639747f4cab1eaf767e25`。同じtreeを持つcheckpoint commitsをGitHub mainの祖先に保全しています。GitHub接続の作成するcommit metadata/parentによりremote commit IDは元のローカルIDと異なりますが、全ファイルの内容・mode・treeの一致を確認しています。

| 元のローカルcheckpoint | GitHub保存commit |
| --- | --- |
| `866b626` | `74303524a3f3e7b21f40d42984da097c7cb12683` |
| `a72421f` | `8954503323f7c71015931aff4f84b9077d1c1947` |
| `7045bd0`（名称・自動session API、tmux時点） | `a91389cd8859016e8e9c9d8c70c760789979029e` |

整理後の唯一の推奨導入は [Rust stdio + Secure MCP Tunnel](../deploy/INSTALL.md)。旧版を必要時に復元して使う場合はその時点の手順を参照し、同じstateのbackendを重複起動しません。旧Library bootstrapの1297a1b指定手順は現行導入に使いません。

旧HTTP配布例は整理しました。既存Rust HTTP/OAuth sourceとRust testは保留機能として保持し、今回のstdio/Tunnel導入では使いません。上流local-mcp原本とLICENSEはそのまま保持しています。
