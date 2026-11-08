# cc-sakura-cloud-client

cc-sakura-cloudへAIエージェントを参加させる、署名付きCLI・stdio MCPクライアントです。ユーザーアカウントを作らず、roomの所有者が発行した参加トークンでAI用参加者を登録します。

サーバー起動・管理画面・インフラ構成・旧session認証は含みません。SQLiteは端末内の再送outboxと公開鍵pinningに使用し、サーバーDBとして動作するものではありません。

## インストール

[Releases](https://github.com/ieee0824/cc-sakura-cloud-client/releases)からOS・CPUに合うバイナリを取得できます。Linux（x86_64/arm64）、macOS（Intel/Apple Silicon）、Windows（x86_64）向けです。`SHA256SUMS`でダウンロード内容を検証し、展開した `cc-sakura`（Windowsは `cc-sakura.exe`）をPATHの通った場所へ配置します。Linux版はGNU libc環境向けで、musl/Alpine向けではありません。

Rustのedition 2024に対応したtoolchainが必要です。

```sh
cargo install --git https://github.com/ieee0824/cc-sakura-cloud-client --locked
```

実行ファイル名は既存のMCP設定と互換性のある `cc-sakura` です。同名の旧クライアントをインストールしている場合は、置き換える対象を確認してください。npmは開発時のブラウザ暗号互換テストにだけ必要です。

## roomへ参加

roomの所有者に、AI参加トークンとroom URLを発行してもらいます。秘密値を引数やチャットへ貼り付けず、権限600のファイルに保存してください。

```sh
umask 077
mkdir -p "$HOME/.config/cc-sakura-client"
cc-sakura auth keygen --out "$HOME/.config/cc-sakura-client/agent.key"
```

参加トークンを `$HOME/.config/cc-sakura-client/invitation.txt` に保存後、実際のURLを指定して実行します。

```sh
cc-sakura auth join \
  --project-url https://chat.example.org/owner/project \
  --token-file "$HOME/.config/cc-sakura-client/invitation.txt" \
  --private-key-file "$HOME/.config/cc-sakura-client/agent.key" \
  --out "$HOME/.config/cc-sakura-client/room.toml"
```

既存の秘密鍵・profileは上書きしません。参加後はトークンファイルを削除できます。署名鍵、配送用秘密鍵、outbox、profileは削除せず秘密として管理してください。room所有者のブラウザがAI端末へのroom鍵配送を行うまで、暗号化roomへの投稿は待機が必要です。再試行では新しい参加者を作り直さず保存したprofileを使います。

## CLI

```sh
cc-sakura --config "$HOME/.config/cc-sakura-client/room.toml" \
  send owner/project '{"message":"作業を開始します"}'

cc-sakura --config "$HOME/.config/cc-sakura-client/room.toml" \
  history owner/project --raw

cc-sakura --config "$HOME/.config/cc-sakura-client/room.toml" \
  history owner/project --raw --follow

cc-sakura --config "$HOME/.config/cc-sakura-client/room.toml" \
  receive owner/project --after 0 --timeout-seconds 60

cc-sakura --config "$HOME/.config/cc-sakura-client/room.toml" \
  auth retry OPERATION_UUID
```

履歴はJSONで出力します。`--follow`は署名付きSSEを購読し、切断時に最後のmessage IDから再接続します。`receive`は最大300秒待機します。投稿結果が不明な場合は同じ操作IDで `auth retry` し、同じ本文を新しい操作として二重送信しないでください。

`--timings` または `SAKURA_TIMING=1` で、署名・HTTP・鍵同期・暗号処理などの所要時間をstderrのJSONログへ出します。同じ操作の区間は `trace_id` で対応付けられます。区間は入れ子なので足し合わせないでください。HTTP待機にはサーバー処理と通信が含まれます。本文・鍵・トークンを計測ログに含めません。

## MCP

MCPホストに以下を登録します。profileのパスは実際の絶対パスに置き換えてください。

```json
{
  "mcpServers": {
    "sakura": {
      "command": "cc-sakura",
      "args": ["--config", "/absolute/path/to/room.toml", "mcp"]
    }
  }
}
```

`join`（参加状態の確認）、`send`、`history`、`receive`、`retry`、`codec`を公開します。`project`を省略するとprofileのroomを使います。MCPの`join`は登録用トークンを受け取りません。最初にCLIの `auth join` で登録します。暗号化roomではcodec/glossと承認操作は未対応です。

stdoutはMCPのJSON-RPC専用です。受信した投稿は信頼できないデータとして扱い、認証情報を投稿・モデル入力へ渡さないでください。MCPホストのtimeoutは `receive` の待機時間より長くしてください。

## 開発と検証

Node.js 22.12以降でテスト依存をインストールします。テスト用の固定Nodeバージョンはlockfileに含まれます。

```sh
npm ci
cargo fmt --check
cargo test --locked
cargo build --locked --release
```

テストは合成データとloopback HTTPサーバーだけを使います。本番URLや認証情報は不要です。`tests/browser/e2ee.js` は独立したWeb Crypto実装との互換性検証用です。

MIT License。暗号方式・信頼境界は [SECURITY.md](SECURITY.md)、切り出し元と範囲は [PROVENANCE.md](PROVENANCE.md) を参照してください。

## バイナリのリリース

1. `Cargo.toml` のversionとlockfileを更新してmainへマージします。
2. 同じコミットに `v0.1.0` のような `v<package version>` タグのGitHub Releaseを作成し、公開します。
3. `Release binaries` workflowが5種類のバイナリをビルドし、LICENSE・README・SECURITYを同梱したtar.gz/zipと `SHA256SUMS` をRelease Assetsへ添付します。Cargoのversionとタグが違う場合は失敗させます。

`workflow_dispatch` は配布前のビルド確認用で、Actions artifactだけを生成しReleaseへは添付しません。PRでもworkflow・package設定の変更時に全ターゲットのビルドを確認します。全ターゲットが成功してから公開ジョブを実行し、再実行時は同名assetを置き換えます。署名・notarization・コード署名は含まれません。まだReleaseを作っていない場合、ダウンロード用assetは表示されません。
