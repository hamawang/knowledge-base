/**
 * R-005：HTML 字符串 → 系统打印对话框 → 用户另存为 PDF。
 *
 * 为什么用 iframe 而不是 window.open / 新窗口：
 *   - window.open 会弹一个新的 webview 窗口，体验突兀且 Tauri 多窗口管理麻烦
 *   - 主窗口 window.print() 会打整个 React 应用 DOM，不是笔记 HTML
 *   - hidden iframe 是浏览器原生支持的打印目标（contentWindow.print），
 *     用户只看到打印对话框，不会看到 iframe 本身
 *
 * 工作流：
 *   1. 创建 hidden iframe，srcdoc 写入 HTML
 *   2. 等 iframe load 事件（HTML 已 inline base64，无网络等待）
 *   3. iframe.contentWindow.print() 触发原生对话框
 *   4. afterprint 事件（或 visibilitychange 兜底）触发后清理 iframe
 *
 * 跨平台：WebView2 / WKWebView / WebKitGTK 都支持 contentWindow.print 与原生
 * "另存为 PDF" 选项，无需额外权限。
 */

const PRINT_FRAME_ID_PREFIX = "kb-pdf-print-frame-";

/**
 * 把 HTML 字符串作为 PDF 打印源。
 *
 * @param html 完整的 HTML 文档字符串（必须是 self-contained，图片已 inline）
 * @param title 用作打印对话框默认文件名（浏览器会自动 sanitize）
 * @returns Promise，在 afterprint 触发或超时兜底后 resolve
 */
export function printHtmlAsPdf(html: string, title: string): Promise<void> {
  return new Promise((resolve, reject) => {
    const iframe = document.createElement("iframe");
    iframe.id = `${PRINT_FRAME_ID_PREFIX}${Date.now()}`;
    iframe.setAttribute("aria-hidden", "true");
    // 不能用 display:none —— 部分 WebView 内核对 display:none 的 iframe
    // 不会触发打印渲染。改用绝对定位 + 0 尺寸 + 视口外移开
    iframe.style.cssText =
      "position:fixed;right:0;bottom:0;width:0;height:0;border:0;visibility:hidden;";

    let cleaned = false;
    const cleanup = () => {
      if (cleaned) return;
      cleaned = true;
      try {
        iframe.parentNode?.removeChild(iframe);
      } catch {
        /* ignore */
      }
    };

    iframe.onload = () => {
      const win = iframe.contentWindow;
      if (!win) {
        cleanup();
        reject(new Error("iframe contentWindow 不可用"));
        return;
      }

      // 设置 iframe document.title 让打印对话框默认文件名跟随笔记标题
      try {
        if (iframe.contentDocument) {
          iframe.contentDocument.title = title;
        }
      } catch {
        /* 跨域情况下访问会抛错，忽略：srcdoc 同源所以一般不会进 catch */
      }

      // afterprint 事件在用户关闭打印对话框后触发（无论确认还是取消）
      const onAfterPrint = () => {
        win.removeEventListener("afterprint", onAfterPrint);
        // 延迟清理：部分内核 print() 调用是异步的，立即移除会打断
        setTimeout(cleanup, 200);
        resolve();
      };
      win.addEventListener("afterprint", onAfterPrint);

      // 兜底：30 秒后强制清理（防止极端情况下 afterprint 未触发导致 iframe 泄漏）
      setTimeout(() => {
        if (!cleaned) {
          cleanup();
          resolve();
        }
      }, 30_000);

      try {
        // 关键调用：必须先 focus 再 print，否则部分浏览器会忽略
        win.focus();
        win.print();
      } catch (e) {
        cleanup();
        reject(e);
      }
    };

    iframe.onerror = (e) => {
      cleanup();
      reject(e);
    };

    // srcdoc 设值后浏览器会异步加载并触发 onload
    iframe.srcdoc = html;
    document.body.appendChild(iframe);
  });
}
