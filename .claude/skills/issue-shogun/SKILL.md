---
name: issue-shogun
description: >-
  このリポ(knmgn/renga)の GitHub issue を、あなた(Claude)が「将軍」(orchestrator)として renga-peers の
  別ペインに立てた「家来」を指揮して 1 issue = 1 worktree で捌く多ペイン・オーケストレーションの手順。
  実装家来は既定で **codex**、レビュー家来は **claude**（静的レビュー）。codex peer が未登録、または
  ユーザーが明示的に望む場合は実装家来も **claude** にする（Claude×Claude 構成）。実装完了後にレビュー
  ループを回し、収束したら実装家来に PR を作成させ、家来ペイン2つを閉じる。ユーザーが「issue を将軍で
  回して」「issue #N を家来にやらせて」「issue-shogun で」等と言ったとき、あるいは複数 issue をペインを
  分けて堅実に捌きたいときに使う。commit / issue コメント / PR コメントなど GitHub 上で消せない記録に
  「将軍」「帝」「家来」等の内輪呼称を絶対に書き込まない鉄則を含む。
---

# issue-shogun — GitHub issue の将軍 × renga 多ペイン・オーケストレーション

## あなたの役割

あなたは **将軍 (orchestrator)**。実装は自分の手では書かず、renga-peers の別ペインに立てた
**実装家来** に投げ、**レビュー家来 (claude)** に検証させ、自分は worktree/issue 管理・ペイン間の
指揮・ユーザーへのエスカレーション・記憶への記録に徹する。将軍が止まると全体が止まるので、将軍は
**確実にツールを呼び**、家来の peer message には即応する。

このリポ自身が renga-peers（多ペイン MCP）の実装元なので、この skill は renga を使って renga の
issue を捌く「セルフホスト」運用になる。ツール名や挙動に不整合を感じたら、それ自体がバグ報告の
種になり得る点も頭に入れておく。

## いつ使うか

- ユーザーが「issue を将軍で回して」「issue #N を家来にやらせて」「issue-shogun で」等と言ったとき
- 複数の GitHub issue を、ペインを分けて堅実に捌きたいとき
- 1セッションの文脈に実装ログを溜めたくない、あるいは実装とレビューを分業したいとき

## 鉄則 0（最優先）: 内輪呼称を GitHub の永続記録に漏らさない

**「将軍」「帝」「家来」**（およびそのローマ字/英訳: shogun, emperor, vassal, retainer 等、この
オーケストレーションが独自に発明した内輪の役職名すべて）は、**消せない／消しにくい記録**に
**絶対に書き込まない**。対象:

- `git commit` のメッセージ（subject・body 両方）
- `git tag` のアノテーション
- `gh issue comment` / `gh issue create`（title・body）
- `gh pr comment` / `gh pr create`（title・body）/ `gh pr review` のコメント本文
- CHANGELOG・リリースノートなど公開読者向けに生成する文章

これらは fork/mirror に残ったり検索エンジンにキャッシュされたりして後から削除しても消えない。
内輪の運用比喩がユーザー以外の目に触れる公開リポジトリの記録に残ることを避ける。

**逆に、使ってよい場所**（将軍とユーザー本人だけが見る内部運用なので問題ない）:

- pane の名前・role（`set_pane_identity`、`spawn_*_pane` の `name=`/`role=` 引数）
- renga-peers 経由の `send_message` / peer message 本文（将軍↔家来間の指示・報告）
- memory への記録
- 将軍からユーザーへのチャット上の報告

**実施方法**: `git commit` / `gh issue comment` / `gh pr create` などを実行する**直前**に、
これから書き込む本文に対象語が入っていないか目視で確認する。見つかったら中立語
（実装担当・レビュー担当・変更内容、など）に置き換えてから実行する。実装家来・レビュー家来への
ブリーフには必ずこの鉄則をそのまま貼り、**家来自身が commit や PR/issue コメントを作る際にも
徹底させる**（家来が将軍の指示を「〇〇将軍より」のように commit message に書いてしまうケースを防ぐ）。

## 実装ロールの選択: Codex か、もう1つの Claude か

既定は **実装家来=codex / レビュー家来=claude**（役割が分かれていてレビューの独立性が高い）。
ただし次のいずれかに当たる場合は **実装家来も claude にする（Claude×Claude 構成）**:

- このマシン/このプロジェクトで `renga-cp mcp install --client codex` が通っておらず、codex が
  peer として参加できない
- ユーザーが明示的に「Codex は使わない」「両方 Claude で」等と指示した
- codex を spawn しても peer 登録が確認できない（`list_panes` / `inspect_pane` で疎通しない）

どちらか不明な場合は、着手前に `AskUserQuestion` で一度確認する（無言で決めない）。

Claude×Claude 構成にする場合の差分:

- 実装家来も `spawn_claude_pane` で立てる（後述のコマンド例を参照）。role 名は衝突しないよう
  `実装家来 (claude-impl)` / `レビュー家来 (claude-review)` のように区別する。
- 実装家来・レビュー家来の両方で **起動警告ゲート**（`--dangerously-load-development-channels`）
  を通す必要がある（Codex にはこのゲートは無いが、Claude ペインには常にある）。
- Codex は使い捨て前提（後述）だが、Claude は `/clear` でセッションを再利用できる。ただし
  1 issue = 1 worktree の原則は変えない（issue をまたいでペインを再利用する場合は `/clear` して
  worktree の cwd を切り替える）。
- レビューの独立性を保つため、実装家来とレビュー家来には**別モデル**を割り当てるのを推奨
  （例: 実装家来=既定モデル、レビュー家来=`model="opus"`）。同じ声で書いて同じ声で審査すると
  自己承認バイアスが乗りやすいため。

## 全体フロー（1 issue = 1 worktree）

各 issue について以下を回す。複数 issue を投げられたら、原則 issue 順に**逐次**で回す
（並列化するとレビュー指摘の交錯や worktree 名衝突が起きやすい。ユーザーが明示的に並列を望む場合のみ、
worktree/家来名を issue# で確実に分離して並行させる）。

1. **将軍**: `gh issue view <N>` で issue 本文を取得し、**スコープを頭に入れる**
   （タイトル・本文・関連ラベル・関連 issue/PR）。issue 内容が曖昧・機構ごと未実装への依存など重大な
   不明点があれば、着手前にユーザーへ `AskUserQuestion` で確認する。
2. **将軍**: worktree 作成
   `git worktree add -b fix/issue-<N> ../renga-issue-<N> main`。
   （ブランチ命名は issue の性質に合わせて `feature/…-<N>` / `fix/…-<N>` / `chore/…-<N>` を選ぶ。
   worktree は `.git` を共有するため、メインの clone で `git config core.hooksPath .githooks`
   が済んでいれば worktree 側でも pre-commit hook（`cargo fmt --all` の強制）はそのまま効く。
   念のため worktree 内で `git config core.hooksPath` を確認する。）
3. **将軍**: 自分に名前を付ける
   `set_pane_identity(target="focused", name="shogun", role="将軍")`。
4. **将軍**: worktree 内に **実装家来**（既定 codex、条件次第で claude）を spawn（後述）。
   初回は worktree 内で `cargo build` を一度通しておく（`target/` は worktree ごとに独立するため
   初回コンパイルに時間がかかる旨を家来に伝える）。
5. **実装家来**: 実装 → `cargo test` / `cargo fmt --all` → **コミット** → 将軍へ完了報告して停止。
6. **将軍**: 実装家来ペインを **水平分割**して **レビュー家来 (claude)** を spawn。
   同じ worktree の cwd で立ち上げる。
7. **レビュー家来 (claude)**: `git diff main...HEAD` を対象に静的レビューでバグ/回帰/スコープ違反を
   洗い出して**report only**（修正はしない）。将軍へ指摘リストを返す。
8. **将軍**: 指摘の finding-validation（最新 HEAD で再現確認できるか）を家来ブリーフに含めて実装家来へ戻し、
   修正→再コミット→レビュー再走査、を繰り返す。**指摘がクリーンになったら**次へ。
9. **実装家来**: 将軍の指示で PR を作成し、PR URL を将軍に報告して停止（**鉄則0のチェック必須**）。
10. **将軍**: 実装家来ペイン・レビュー家来ペインの **両方を close** し、ユーザーに完了報告
    （PR URL・commit hash・レビューで直した主要な指摘の要約）。

## 家来ペインの起動と通信（renga-peers）

renga-peers のツールは deferred。まず `ToolSearch` で
`spawn_claude_pane / spawn_codex_pane / list_panes / inspect_pane / send_keys / send_message /
set_pane_identity / check_messages / close_pane` を読み込む。

### 実装家来 (codex) の spawn — 既定パターン

```
spawn_codex_pane(
  direction="vertical",           # 将軍の隣に立てる
  target="focused",
  cwd="<worktree の絶対パス>",
  name="worker-<N>",
  role="実装家来 (codex)"
)
```

- `spawn_codex_pane` は既に `RENGA_PEER_CLIENT_KIND=codex` の MCP 登録が済んでいる前提。
  `renga-cp mcp install --client codex` を通していれば、plain `codex` 起動で peer 参加できる。
  未登録なら「Claude×Claude 構成」に切り替える（前述）。
- Codex には Claude の起動警告ゲートは基本無いが、初回に trust prompt が出ることがある。
  `inspect_pane(target)` で確認し、必要なら `send_keys(target, keys=["Enter"])` で通す。
- **peer message の受け方が違う**: Codex 宛の `send_message` は renga がペイン nudge を出し、Codex は
  `check_messages` を叩いて本文を取りに来る。将軍から見ると `send_message` の呼び方は同じでよい。

### 実装家来 (claude) の spawn — Claude×Claude 構成のとき

```
spawn_claude_pane(
  direction="vertical",
  target="focused",
  cwd="<worktree の絶対パス>",
  name="worker-<N>",
  role="実装家来 (claude-impl)",
  permission_mode="bypassPermissions"
)
```

- spawn 直後は起動警告ゲートで止まる。`inspect_pane(target)` で確認し
  `send_keys(target, keys=["Enter"])` で抜ける。**ブリーフはゲート突破後に送る**（突破前に送ると届かない）。

### レビュー家来 (claude) の spawn

実装家来ペイン (`worker-<N>`) を **水平分割**して、同じ worktree でレビュー家来を立てる:

```
spawn_claude_pane(
  direction="horizontal",         # 実装家来の下に並べる
  target="worker-<N>",
  cwd="<worktree の絶対パス>",
  name="reviewer-<N>",
  role="レビュー家来 (claude)",   # Claude×Claude 構成では "レビュー家来 (claude-review)"
  permission_mode="bypassPermissions",
  model="opus"
)
```

- **起動警告を抜ける（重要）**: spawn 直後は起動警告で止まっている。`inspect_pane(target)` で確認し
  `send_keys(target, keys=["Enter"])` で抜ける。**ブリーフは警告突破後**に `send_message` で送る。

### 状態確認・応答

- `inspect_pane(target, lines=N)` でフッターを見る。`Thinking...` / `esc to interrupt` で作業中、
  空プロンプトで待機中。週次リミット警告(`You've used N% of your weekly limit`)もここで見る。
- 家来からの報告は peer message (`<channel source="renga-peers" ...>`) として届く。**即応する**。

> ツールタグ崩れに注意: `<invoke>` を地の文に書くと「tool call was malformed」になり将軍が止まる。
> ツールは必ず正規のツール呼び出しとして発行する。止まったら全体が止まる。

## 実装家来への初回ブリーフ（issue 単位で自己完結）

家来はセッションを持たない前提（Codex は必ず、Claude×Claude 構成でも issue ごとに前提を毎回
明示する）で毎回フルの自己完結ブリーフを送る。`send_message(to_id="worker-<N>", message=...)`
の本文に以下を全部入れる:

- **役割**: あなたは実装家来。報告は `send_message(to_id="shogun", message=...)` で返す。
  疑問があれば実装を進める前に shogun に peer message で相談する（勝手にスコープを広げない）。
- **プロジェクト背景**:
  - リポ: `knmgn/renga`（このリポ）。Rust の TUI アプリ（ratatui + crossterm + portable-pty + vt100）。
  - worktree の**絶対パス**とブランチ、現在 HEAD
  - 初回は worktree 内で `cargo build` を一度通す（依存取得とコンパイルで時間がかかる）
  - ルートの `CLAUDE.md` に **Fork Identity** の指定がある: コマンド名は `renga-cp` に変わったが、
    `renga-peers`（MCP サーバー名）、`RENGA_*` 環境変数、`~/.config/renga/`、IPC ソケットディレクトリ、
    layout TOML のキー、および地の文の product 名 `renga` は**意図的に upstream と同一**にしてある。
    これらを「直す」リネームは**しない**。同様に、Issue #102 のリネーム後に残っている `ccmux` 言及
    （upstream attribution・`Cargo.toml` の version-history コメント・`.claude/` 配下）も意図的なので
    スコープ外の掃除をしない。
- **今回の issue（本文丸ごと）**:
  - `gh issue view <N> --json number,title,body,labels,url` の出力から number/title/body/labels/url を貼る
  - 「この issue **だけ**を対象にする。次の issue には進まない」と釘を刺す
- **鉄則**（毎回貼る）:
  - **スコープ厳守**: issue に書かれた課題だけを直す。投機的改善・無関係リファクタ・周辺のスコープ外拡張はしない。
  - **finding-validation**: 着手前に最新 HEAD で再現確認する。既に他コミットで直っている・スコープ外・
    テストで無効化済みなら、修正せず non-actionable として理由を添えて将軍へ報告
    （将軍が issue クローズ or 差し戻しを決める）。
  - **回帰なし**: `cargo test` 緑・`cargo build` クリーン。可能なら該当挙動の最小回帰テストを追加。
  - **フォーマット**: コミット前に `cargo fmt --all` を通す（pre-commit hook が有効ならこれで強制されるが、
    hook が無効な worktree では自分で明示的に走らせる）。CI の `rustfmt` job が未整形コミットで即fail するため。
  - **本質的衝突は将軍にエスカレ**: 機構ごと未実装・既存挙動の削除/変更が必要・他の正(仕様/設計)との衝突は
    家来判断で強行しない。`send_message(to_id="shogun", ...)` で状況と選択肢案を上げて指示を待つ。
  - **鉄則0（内輪呼称の漏洩防止）**: commit message に「将軍」「家来」等の内輪呼称やその英訳
    （shogun/vassal/retainer 等）を**絶対に書かない**。commit message は普通の変更内容の説明だけにする。
- **完了ゲート**:
  - 実装 → `cargo test` / `cargo build` 緑 → `cargo fmt --all` → **コミット**
    （メッセージは `type(scope): summary` 形式。scope は fork 固有の変更なら `renga-cp`、
    upstream と共有する挙動なら `renga`。issue# を本文か脚注に含める）
  - **コミットは意味のあるまとまりごとに分割してよい**（1 issue = 1 commit に縛らない）。
    ロジック修正・テスト追加・関連リファクタなど論理単位が違うものは分ける。粒度は「レビューで
    その単位だけを差し戻せるか」を目安に。
  - 報告に含める: 実施内容の要約 / 変更ファイル一覧 / **コミットのリスト（hash + 一行要約）** /
    再現→解消の証跡（該当があれば）
  - 週次リミット警告が出たら**即停止**・部分でもコミットしてから報告
- **PR はまだ作らない**: レビュー完了後、将軍からの指示で作成する（後述の PR ブリーフを別途送る）。

Codex はメッセージ受信のため `check_messages` を叩く必要がある。ブリーフ末尾に
「メッセージ受信は `check_messages` で行う」と明記しておくと確実（Claude×Claude 構成では peer message
は自動で届くので不要）。

## レビュー家来 (claude) へのブリーフ

`send_message(to_id="reviewer-<N>", message=...)` で以下を送る:

- **役割**: あなたはレビュー家来。報告は `send_message(to_id="shogun", ...)` で返す。修正はしない。
- **対象**: この worktree の **branch diff**。`git diff main...HEAD` を確認範囲とする
  （`git diff main..HEAD` は使わない。current tip の main を巻き込むため）。
- **タスク**: ルートの `CLAUDE.md` が定める評価基準に沿って静的レビューする。この Rust TUI アプリは
  Playwright MCP が使えないため、diff 分析・エッジケース・ロジックの正しさ・キーバインド衝突・
  レイアウト計算（binary tree のペイン分割・resize 計算）の整合性を中心に、時間を惜しまずバグを
  見つけまくる。指摘だけで修正はしない。
- **今回の issue（本文丸ごと）**: `gh issue view <N>` の出力を貼り、「スコープ完了判定の唯一のソース」と明示。
- **報告フォーマット**（毎回同じにする）:
  - GO / NEEDS_FIX を先頭に一行で
  - `NEEDS_FIX` の場合は指摘リストを深刻度(BLOCK/MAJOR/MINOR)＋根拠(該当 file:line / 期待挙動 / 現状挙動)
    の3点セットで列挙
  - スコープ違反の疑いがあるものは `SCOPE?` タグを付ける（将軍が裁定）
- **観点**:
  - (1) issue に書かれた課題が最新 HEAD で解消しているか（before=再現 / after=解消）
  - (2) 回帰なし（`cargo test` 緑・周辺挙動不変）
  - (3) スコープ厳守（この issue の課題だけ・投機的改善なし）
  - (4) 品質: 明らかなバグ / キーバインド衝突 / レイアウト計算のズレ / `unsafe` の不要な追加 / 命名やコメントの誤り
  - (5) **Fork Identity 違反の有無**: `renga-peers` / `RENGA_*` / `~/.config/renga/` / layout TOML キー /
    地の文の `renga` を「直して」いないか（CLAUDE.md が意図的固定と明記している）
- **週次リミット警告が出たら即停止して部分報告**、も毎回入れる。

## レビューループ

1. 実装家来の**完了報告**を受けたら、レビュー家来に上記ブリーフで検証依頼。
2. レビュー家来から `GO` が返れば PR フェーズへ。`NEEDS_FIX` なら指摘を実装家来へ**そのまま転送**し、
   将軍側で以下を明示する:
   - 各指摘について **finding-validation**（最新 HEAD で再現確認）してから修正
   - スコープ外(`SCOPE?`) は将軍が裁定して指示を出す（採用/繰延/フォローアップ issue 化）
   - **修正は指摘の粒度に合わせてコミットを分ける**（1 指摘 = 1 commit を基本、密接に関連するものだけ
     まとめる）。全指摘を1つの巨大コミットに詰めない。理由: (a) 次周のレビュー家来が
     「どの指摘にどの commit が対応したか」を対応付けやすい、(b) 差し戻しや squash の判断が PR 側で楽、
     (c) commit log が「レビュー→修正」の履歴として残る。
   - 完了報告には**この周で追加したコミットのリスト（hash + 対応した指摘番号/要約）**を必ず含める
3. 実装家来の再報告を受けたら、レビュー家来には **`/clear` せず** 同じセッションのまま再レビューを依頼する
   （Claude×Claude 構成でも同様）。前回自分が出した指摘リストと、実装家来がそれをどう直したかの差分を
   覚えていてほしいため。再依頼メッセージには「最新 HEAD (`git log -1`) を確認し、前回自分が挙げた指摘の
   解消状況を1件ずつ判定→新規の指摘があれば追加、で GO / NEEDS_FIX を返す」とだけ書けばよい
   （issue 本文や観点は初回ブリーフに既に載っている）。
4. `GO` が返るまでループ。**上限3〜4周を目安**にし、それを超えたら将軍がユーザーへ現状報告して裁定を仰ぐ
   （指摘が本質的にスコープ外/仕様相談が必要なケース）。
5. **収束後・次 issue に移る前**にレビュー家来を close（次 issue では新しい reviewer ペインを立てる）。
   `/clear` はしない。同一 issue のレビュー context は issue 完了と同時に破棄する。

## PR 作成フェーズ

レビューがクリーンになったら、実装家来へ `send_message(to_id="worker-<N>", ...)` で PR 作成ブリーフを送る:

- **タイトル・本文の言語・書式は既存のコミット履歴の慣習に合わせる**: このリポは
  `type(scope): summary (#N)` 形式・英語が既定（`git log --oneline` で確認できる）。
  ユーザーから別言語/別書式の指定がない限りこれに合わせる。
- 本文の構成:
  - 概要（何を、どう直したか）
  - 対応した issue: `Closes #<N>`
  - 変更点の要約（ファイル群・主要変更）
  - 動作確認（実行したテスト・`cargo test`/`cargo build` の結果など）
  - レビューで直した主要指摘があれば箇条書き
- **鉄則0を PR 作成の直前に再チェックする**: title・body に「将軍」「家来」「帝」やその英訳
  （shogun/vassal/retainer/emperor）が入っていないか確認してから `gh pr create` を実行する。
  入っていたら中立的な表現に書き換える。
- **UTF-8 安全**な作成手段を使う（下記「UTF-8 安全な gh issue/PR」）。特に日本語を含む issue 本文を
  引用する場合、シェル経由の `echo` 直接埋め込みは禁止。
- 作成後は `gh pr view <URL> --json title,body -q .` を UTF-8 ファイルへ落として **Read ツールで検証**し、
  文字化け・literal `\n` 混入・鉄則0違反の三点を確認してから将軍へ URL を報告。

## クリーンアップ

- PR URL を受け取ったら、`close_pane(target="reviewer-<N>")` → `close_pane(target="worker-<N>")` の順で
  両方閉じる。
- 将軍はユーザーへ完了報告: **PR URL / commit hash / レビューで直した主要指摘の要約**。
- worktree 自体は残しておく（PR merge 後にユーザー判断で `git worktree remove ../renga-issue-<N>`
  と `git branch -D fix/issue-<N>` を促す）。

## スコープ衝突のエスカレーション（将軍が握る最重要判断）

家来が「機構ごと未実装への依存」「既存挙動の削除/変更が必要」「他の正(仕様/設計)との衝突」など本質的衝突に
当たったら、家来に勝手に判断させない。家来は将軍へエスカレし、**将軍が `AskUserQuestion` で選択肢
(推奨案を先頭)を提示して裁定**を仰ぐ。裁定後:

- 裁定内容を家来へ伝達して続行。
- 将来も効く方針は **memory** に書き、以後は家来が自走できるようにする。
- 繰延・別対応は**フォローアップ issue 化**してトレース可能にする。

## 週次リミット管理と resume point

家来ペインのフッターに `You've used N% of your weekly limit · resets <time>`。将軍からはプログラム的に
読めないので `inspect_pane` で目視、危険域ではユーザーに残量確認を依頼する。

- 危険域では、進行中のレビューだけ走り切らせ**新規 issue は着手しない**。
- 家来には常に「リミット警告が出たら即停止・部分でもコミットしてから報告」と指示。
- 停止時は **resume point を記憶へ**: 対象 issue# / worktree ブランチ / HEAD /
  「実装完了・レビュー n 周目・PR 未作成」等の状態 / 再開手順（家来ペインが残れば再利用、無ければ spawn し直し）。

## UTF-8 安全な gh issue/PR

シェル/中間ツールが日本語や絵文字、改行を壊すことがある。`echo` 直接埋め込み・`--body "…"` は
文字化け・literal `\n` 混入を起こしがち。**python + `gh api` に JSON を渡す**方式を推奨:

```python
import json, subprocess
payload = {"title": title, "body": body}   # body は UTF-8 文字列
subprocess.run(
  ["gh","api","-X","POST",f"repos/{REPO}/pulls","--input","-"],
  input=json.dumps(payload), text=True, encoding="utf-8"
)
```

代替: **UTF-8 一時ファイル**に本文を書き出し、`gh pr create --body-file <path>` で渡す。
どちらでも作成後は `gh pr view <URL> --json body -q .body > file` でファイルに落として
Read ツールで検証する（パイプ経由のコンソール表示は文字コード変換で信用できない）。**この検証時に
鉄則0（内輪呼称の漏洩）も同時にチェックする。**

## 記憶(memory)の活用

将軍は判断・方針・進捗を memory に残す:
- project 方針（このリポ固有の運用・確定ルール）
- feedback（ユーザー指示の why と適用法）
- resume point（残 issue・worktree・進行フェーズ）

関連メモは `[[name]]` でリンクする。issue-shogun のセッションで何度も引くことになるプロジェクト固有の
ルール（例: Fork Identity の固定範囲・cargo fmt pre-commit hook の有無など）は既存の memory を活用し、
必要に応じて更新する。

## 注意（Codex 家来 特有）

- **Codex は Claude のような `/clear` を持たない**。1 issue = 1 codex ペインで使い捨て前提。ペイン閉じで初期化。
  したがってレビュー修正ループ中は同じ codex セッションを継続して使う（都度フルブリーフを送るのではなく、
  増分の「指摘リスト＋修正指示」だけを送ってよい）。
- **peer message は nudge → check_messages** で読む。ブリーフ末尾に `check_messages` を叩くよう促す一文を
  入れておくと確実。
- **codex 経由のコミット・push には自プロジェクトの pre-commit hook（`cargo fmt --all` 強制）が走る**。
  将軍側でシェル経由の `git` を代行しない（家来ペインの cwd と env で走らせる）。
