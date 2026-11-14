# Index - {{today}}

ワークスペースを開いたとき、`index.md` の作成日・更新日が今日でなければ、
このテンプレートから `index.md` が作り直されます（今日すでにある場合は上書きされません）。

## 今日（{{today}}）が期限・予定のタスク

{{task:priority(today)}}

## 1 週間以内に期限・予定のあるタスク（見出しなし）

{{task(today+7)}}

## リンク

- [[Templates/template_daily.md]]
