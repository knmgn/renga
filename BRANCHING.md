# Branching & Divergence Policy

このリポジトリ (`knmgn/renga`) は [`suisya-systems/renga`](https://github.com/suisya-systems/renga) のフォークで、GitHub Copilot CLI 対応を独自に足したラインです。上流の renga 自体も [`Shin-sibainu/ccmux`](https://github.com/Shin-sibainu/ccmux) から派生しているため、系譜は 3 段になります。

```
Shin-sibainu/ccmux  →  suisya-systems/renga  →  knmgn/renga (このリポジトリ)
```

インストールされるコマンドは `renga-cp` で、上流の `renga` と同じマシンに共存できます。ただし共存には 1 つ制約があります: MCP サーバ名 `renga-peers`・環境変数 `RENGA_*`・設定ディレクトリ `~/.config/renga/` は上流と共有したままなので、`renga mcp install` と `renga-cp mcp install` は同じ登録エントリを奪い合います。名前空間を分けなかったのは、`renga-peers` が Claude の `<channel source="renga-peers">` の導出元であり、`docs/api-surface-v1.0.md` が改名を破壊的変更として凍結しているためです。

バージョン同期や定期的な上流取り込みは行っておらず、上流から有用な変更があれば必要に応じて cherry-pick する程度です。

## ブランチ構成

| ブランチ | 役割 | push 権限 |
|---|---|---|
| `main` | **renga の本流** (default branch、リリース対象) | PR のみ。force-push 禁止 |
| `master` | **上流ミラー (任意保守)**。必要なときだけ FF 追従するスナップショット用 | force-push 許可 (上流が rebase する場合に備え) |
| `feat/*`, `fix/*`, `chore/*` | 通常の機能ブランチ。`main` から切る | PR で `main` にマージ |
| `upstream-pr/*` | 上流に還元したい変更があれば切る (基本は使わない) | `suisya-systems/renga` へ PR |

### `master` を残しておく理由

- 過去の上流 commit と現在の renga の差分を git で直接比較したいときに便利
- 上流から個別に cherry-pick する場合のベースになる
- アクティブに追従しているわけではないので、`master` 自体には機能を足さない

## Remote

```bash
git remote -v
# origin    git@github.com:knmgn/renga.git          ← このフォーク (主開発ライン)
# upstream  https://github.com/suisya-systems/renga ← 直上の分岐元
```

さらに元の ccmux を見たいときだけ、3 つ目を足します。

```bash
git remote add ccmux https://github.com/Shin-sibainu/ccmux.git
git fetch ccmux
```

通常開発には `origin` だけあれば足ります。

## 日常運用

### 通常の機能開発

```bash
git checkout main
git pull
git checkout -b feat/xxx
# ... 実装 ...
gh pr create --base main
```

これが基本フロー。renga の開発は `main` 中心で完結します。

### 上流から cherry-pick する (任意・必要時のみ)

定期的な「上流 sync」は行いません。renga と upstream ccmux はもう独立した実装系列です。
ただし、上流に明らかに有用なバグ修正や小さな改善があれば、その単発 commit を選んで取り込むことはあります。

```bash
git fetch upstream

# 1. master を upstream/master に FF 追従 (記録用スナップショット)
git checkout master
git merge --ff-only upstream/master
git push origin master

# 2. 取り込みたい commit を main に cherry-pick
git checkout main
git pull
git checkout -b chore/cherry-pick-<topic>
git cherry-pick <upstream commit>
gh pr create --base main --title "chore: cherry-pick <topic> from upstream renga"
```

renga 側の実装と衝突することが多いので、コンフリクトが大きい場合は cherry-pick せず renga 流で書き直すのが基本方針です。

### 上流に PR を返す (基本ユーザーから明示指示があった場合のみ)

renga の独自実装から汎用的に切り出せるものを上流に還元したいケースは稀ですが、出す場合は `master` を base にした別ブランチで送ります。
`main` から直接 PR すると無関係の renga 独自変更が混入するので厳禁。

```bash
git fetch upstream
git checkout master
git merge --ff-only upstream/master
git checkout -b upstream-pr/foo
git cherry-pick <main の commit>
git push origin upstream-pr/foo
gh pr create --repo suisya-systems/renga --base main
```

ユーザーから明示の指示がない限り、上流 PR は提案・実行しません — 上流の受け入れタイミングに renga の進捗が縛られないようにするための方針です。

## リリース

- **配布は GitHub Release のみ。** npm には publish しません。上流の publish は npm Trusted Publishing に依存しており、これは上流のパッケージとリポジトリに紐付いているためフォークに引き継げません。毎タグ失敗するジョブを残すより外すほうが良いので、`publish-npm` ジョブは `.github/workflows/release.yml` から削除してあります。
- **Git tag**: 通常の semver (`vX.Y.Z`)。上流 renga のバージョン番号とは同期させません。
- **Prerelease が必要な場合**: `vX.Y.Z-rc.N` / `-beta.N` 等を使用。`contains(github.ref_name, '-')` で workflow が自動的に prerelease 扱いにします。
- **成果物名**: `renga-cp-windows-x64.exe` / `renga-cp-macos-x64` / `renga-cp-macos-arm64` / `renga-cp-linux-x64` + `checksums.txt`。
- **アップデート確認**: `src/version_check.rs` が GitHub Releases API (`repos/knmgn/renga/releases/latest`) の `tag_name` を読みます。上流は npm registry を見ていました。

## ブランチ保護

**このフォークには現在ブランチ保護を設定していません。** 保護設定やルールセットはフォークに継承されないため、上流の「main は PR 必須」はここには効いていません。単独メンテナのリポジトリなので `main` へ直接 push できます。

とはいえ運用としては PR 経由を推奨します。CI (`.github/workflows/ci.yml`) は `pull_request` と `main` への push の両方で走るので、PR にすればマージ前に rustfmt / clippy / 3 プラットフォームの test を通せます。強制したくなったら上流と同じ設定を入れてください:

- `main`: PR 必須 / CI 必須 / force-push 禁止 / 直 push 禁止

## 関連

- リリース手順の詳細は `CLAUDE.md` の Release Process を参照
- (内部) `.claude/skills/upstream-sync/` Skill は本ドキュメントの divergence policy に追随しており、ad-hoc な cherry-pick / 逆 PR 手順の補助に用途を絞っています。本 BRANCHING.md が常に正本です
