# タスクの書き方サンプル

Obsidian Tasks と同じ書式です。タスク行でスペースを押すと、記号を選ぶ入力補助が開きます。
チェックボックスは `- [ ]` のあとに**空白が必要**です。`- [ ]月曜` のように詰めるとタスク扱いになりません。

## 基本

- [ ] 普通のタスク
- [x] 完了したタスク ✅ 2026-11-10

## 日付

- [ ] 期日（deadline） 📅 2026-11-14
- [ ] 予定日（取りかかる日） ⏳ 2026-11-12
- [ ] 開始日（これより前は着手不可） 🛫 2026-11-20
- [ ] 作成日つき ➕ 2026-11-01 📅 2026-11-30

## 優先度

- [ ] Highest 🔺
- [ ] High ⏫
- [ ] Medium 🔼
- [ ] Normal（記号なし）
- [ ] Low 🔽
- [ ] Lowest ⏬

## 繰り返し（チェックすると次回分が作られる）

- [ ] 毎日 🔁 every day 📅 2026-11-14
- [ ] 平日のみ 🔁 every weekday ⏳ 2026-11-14
- [ ] 毎週月曜 🔁 every week on monday 📅 2026-11-17
- [ ] 月・金 🔁 every monday, friday ⏳ 2026-11-14
- [ ] 2 週間ごと 🔁 every 2 weeks 📅 2026-11-28
- [ ] 毎月 15 日 🔁 every month on the 15th 📅 2026-11-15
- [ ] 毎月末 🔁 every month on the last day 📅 2026-11-30
- [ ] 毎月第 1 月曜 🔁 every month on the 1st monday 📅 2026-12-07
- [ ] 毎年 🔁 every year 📅 2027-04-01
- [ ] 完了した日から 7 日後 🔁 every 7 days when done 📅 2026-11-14

## 完了時の動作

- [ ] 完了したら行ごと削除 🏁 delete 📅 2026-11-14

## 依存関係

- [ ] 先にやること 🆔 prep1
- [ ] 後でやること（prep1 が終わるまで待つ） ⛔ prep1

## コードブロックの中はタスク扱いされない

```markdown
- [ ] これは集計されない 📅 2026-11-14
```

## 一覧展開（テンプレートの中で使う）

```
{{task:priority(today)}}          今日以前（期日 or 予定日）を優先度別に
{{task:priority(2026-11-30)}}     指定日以前
{{task:priority(today+1)}}        明日以前（today-1 / yesterday / tommorow も可）
{{task:high(today)}}              High のみ（highest/medium/low/lowest/none も可）
{{task(today)}}                   見出しなしで全部
{{task:priority}}                 日付を問わず全未完了
{{task}}                          日付を問わず全未完了（見出しなし）
```
