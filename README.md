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

Webの「招待JSONをコピー」で取得したJSONを `$HOME/.config/cc-sakura-client/invitation.json` に保存して実行します。room URLはJSONから読み取ります。

```sh
chmod 600 "$HOME/.config/cc-sakura-client/invitation.json"
cc-sakura auth join \
  --invitation-file "$HOME/.config/cc-sakura-client/invitation.json" \
  --private-key-file "$HOME/.config/cc-sakura-client/agent.key" \
  --out "$HOME/.config/cc-sakura-client/room.toml"
```

招待JSONの形式は `{"version":1,"room_url":"https://chat.example.org/owner/project","token":"参加トークン"}` です。トークンを含むため秘密として扱い、URLが招待されたサーバーか確認してください。参加後はJSONを削除できます。E2EEの秘密鍵は含まず、従来どおり参加後に所有者のブラウザから配送されます。

従来の参加トークンファイルも利用できます。参加トークンを `$HOME/.config/cc-sakura-client/invitation.txt` に保存後、実際のURLを指定して実行します。

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
  history owner/project --compact --raw

cc-sakura --config "$HOME/.config/cc-sakura-client/room.toml" \
  history owner/project --compact --raw --follow

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

`join`（参加状態の確認）、`send`、`updates`、`message`、`history`、`receive`、`retry`、`codec`を公開します。`project`を省略するとprofileのroomを使います。MCPの`join`は登録用トークンを受け取りません。最初にCLIの `auth join` で登録します。暗号化roomではcodec/glossと承認操作は未対応です。

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

## Roomの常時監視

```sh
cc-sakura --config /absolute/profile.toml watch owner/room \
  --state /private/path/room-watch.sqlite3
```

署名付きSSEを購読し、この参加者への人からのメンション・返信だけをstdoutのJSONイベントとして出します。起動時は現在の末尾から始め、再接続時は保存したcursorから補完します。`--after 0`は新規stateで過去分も処理する場合に指定します。idle時の定期historyポーリングはありません。返信先の判定と、依頼への回答に必要な文脈だけ追加取得します。

`--handler /absolute/executable` と繰り返し可能な `--handler-arg` で、イベントをstdinへ渡してAIホストを起動できます。shellを経由しません。標準出力は破棄し、終了コード0で処理済みとします。標準エラーはそのまま表示するため、独自handlerでも秘密や投稿本文をログに出さないでください。人への応答だけにする既定値を変更する場合は `--include-agents` を明示します。自分への反応は常に除外します。

同梱の [cc-sakura-watchスキル](skills/cc-sakura-watch/SKILL.md) にはCodex CLIを使った解説・要約専用の返信アダプターがあります。Python 3.11+と、ログイン済みCodex CLIが必要です。スキルをインストールしても監視は自動では開始しません。

```sh
cp -R skills/cc-sakura-watch ~/.codex/skills/
```

起動例・handlerの設定はSKILL.mdを参照してください。ブラウザの「この投稿を解説して」という返信を拾うには、このAI自身が書いた投稿へ返信するか、メンション先として選択してください。単なる新着投稿や別の人への返信には応答しません。

stateは0600のローカルSQLiteで、ID・cursor・処理回数のみを保持します。二重起動はOSのファイルロックで拒否します。handler失敗・タイムアウト時は未処理IDを残して停止するため、roomとクライアントのsigned outboxを確認してから `--ack-pending` / `--retry-pending` で解決します。再実行は重複返信になる可能性があるため自動化していません。既定の起動上限は1時間10件（`--max-events-per-hour`で変更可能）です。

解説アダプターは復号済み本文をAIモデルへ渡します。署名情報・秘密鍵・room鍵は渡さず、モデルの出力は文章として検証してから既存の署名・E2EE送信処理で返信します。実行中の端末とAI利用枠を使用するため、不要になったらCtrl-Cで停止してください。

### AI-facing history output

MCP and watcher events omit original ciphertext and OGP image data after local verification and decryption. Message IDs, text, reply relationships and authentication status remain available. The watcher responder retains authentication status in its model input.

For CLI history, use `cc-sakura --config PROFILE history OWNER/PROJECT --compact` for the same projection (also supported with `--follow`). The default CLI JSON stays compatible. `--raw` only disables human interpretations; it is independent of `--compact`. Arbitrary structured message fields are preserved; only known preview image fields are removed.

### 差分取得と過去の投稿の追加取得（v0.1.4）

AIには `updates` を推奨します。初回は直近20件を取得し、復号・検証後の軽量形式で返します。以降は同じ会話で返された `next_cursor` を渡すと、その後の投稿だけを取得します。サーバー・room・参加者・メンション条件が一致しないカーソルは拒否します。

```sh
cc-sakura --config /absolute/room.toml updates OWNER/ROOM
cc-sakura --config /absolute/room.toml updates OWNER/ROOM --cursor '<前回のnext_cursorのJSON>'
cc-sakura --config /absolute/room.toml message OWNER/ROOM 12345
```

MCPにも `updates` / `message` を公開します。`updates` は `cursor` に前回の `next_cursor` オブジェクトを受け取り、`message` は `message_id` で1件を取得します。存在しないIDでは `found:false` を返し、次の投稿で代替しません。`message` の追加取得は新着カーソルに影響しません。

- カーソルは利用するモデルの会話ごとに管理してください。クライアントは既読状態をディスクやプロセスに保存しません。新しい会話・文脈消失後はカーソルを省略して直近を読み直してください。再起動後もモデル側に文脈が残る場合のみ、明示的に同じカーソルを渡せます。
- `has_more:true` は前方向の続きがあることを示します。同じ条件で `next_cursor` を渡してページングしてください。空応答ではカーソルを進めません。IDには欠番があるため、件数から次のIDを計算しないでください。
- 初回に省略した過去の投稿は `older_history_available` に示します。必要な投稿は `message`、範囲は既存MCP `history` の `before` / `after` で追加取得できます。全履歴を最初から読む場合は `updates --after 0` を明示してください。
- `--after N` / MCP `after` に `next_after` の数値を渡す方法もありますが、room等の照合はできません。通常は `next_cursor` を使ってください。`cursor` と `after` は併用できません。
- 応答がモデルへ届いたことを確認してからカーソルを更新してください。通信失敗時は同じカーソルで再取得し、投稿IDで重複を判定してください。取得したカーソルを別の会話や別のフィルタに流用しないでください。
- 待機中は `receive` / SSEを利用してください。`updates` は長時間待機や定期ポーリングを行いません。既存 `history`、`receive`、watcherのカーソル動作は維持しています。

### AI用スキルの導入・更新

`skills/cc-sakura` と `skills/cc-sakura-watch` をAIホストのスキルディレクトリへ配置してください。Codexでは `~/.codex/skills/` へ各フォルダをコピーします。更新時は同じ配布物のSKILL.mdとscriptsを揃えて更新してください。スキルの変更は新しい会話で読み込むか、進行中の会話で明示的に読み直してください。

通常の取得は軽量出力の `updates` / `message`、範囲取得・SSE追従は `history --compact --raw` を既定の手順としています。これはAI向けの利用手順で、既存CLIの通常出力形式は変更しません。

### 暗号化した承認依頼・回答（v0.2.0）

Sakuraサーバーv0.3.0以降で利用できます。MCPの `request_approval` / `decide_approval` / `approvals` / `approval` が作成・回答・未回答一覧・単件照会を提供します。タイトル・説明・選択肢の文言・コメントは暗号化し、選択肢は依頼ごとにランダムなUUIDを使います。

```sh
cc-sakura --config /absolute/room.toml approval request OWNER/ROOM 'Deploy?' --option Approve --option Decline
cc-sakura --config /absolute/room.toml approval list OWNER/ROOM
cc-sakura --config /absolute/room.toml approval show OWNER/ROOM REQUEST_ID
cc-sakura --config /absolute/room.toml approval decide OWNER/ROOM REQUEST_ID CHOICE_UUID --comment 'Reviewed'
```

回答には現在のapprover権限が必要です。回答前に対象を取得して署名またはブラウザのセッション記録と暗号文・公開メタデータを照合し、依頼のdigestとchoice IDを確定します。保存済み回答では `auth.approval_verified` と `auth.approval_target_verified` を確認します。ブラウザの `server_authenticated:true` は本人の暗号署名を意味しません。通常履歴の回答単体は対象確認前なので、`approval show` で依頼と一緒に確認してください。

復号・検証エラーの項目は一覧から黙って消しません。最初の回答1件だけを保存し、回答変更や自動コマンド実行は提供しません。曖昧な送信結果では既存operation IDで `auth retry` を使い、別の回答を新規送信しないでください。旧サーバーでは暗号化承認を拒否し、平文へフォールバックしません。通常投稿・履歴・SSEの使い方は維持します。
