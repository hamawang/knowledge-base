/**
 * 对比/合并视图里取笔记 markdown 的工具。
 *
 * tiptap-markdown 把连续空段落序列化成 HTML 兜底（`<p><br></p>` / `<p></p>`）——纯 markdown 没法
 * 表达"多个连续空行"。这在 diff 视图里又丑又容易被误以为是坏的，所以统一 tidy 掉再展示。
 */
import type { Editor } from "@tiptap/react";

/** 清理 tiptap-markdown 序列化里的空段落 HTML 兜底，并把连续空行压成一个 */
export function tidyNoteMarkdown(md: string): string {
  return md
    .replace(/<p>\s*(?:<br\s*\/?>\s*)?<\/p>/gi, "") // 去掉 <p></p> / <p><br></p>
    .replace(/\r\n/g, "\n")
    .replace(/[ \t]+\n/g, "\n") // 行尾空白
    .replace(/\n{3,}/g, "\n\n") // 多个连续空行 → 一个
    .trim();
}

/** 取当前编辑器内容的 markdown（已 tidy）；无 markdown storage 时退回纯文本 */
export function getNoteMarkdown(editor: Editor): string {
  const storage = editor.storage as { markdown?: { getMarkdown: () => string } };
  const raw = storage.markdown?.getMarkdown() ?? editor.getText({ blockSeparator: "\n\n" });
  return tidyNoteMarkdown(raw);
}
