import { useEffect, useMemo, useState } from "react";
import { Spin, Alert, Tabs, Table, Tag } from "antd";
import { attachmentApi, type ExcelPreviewData } from "@/lib/api";

interface Props {
  /** kb-asset:// 相对路径 */
  rel: string;
}

/**
 * Excel/ODS 附件预览。
 *
 * 复用后端 excel_parser（calamine），输出结构化 sheets/headers/rows，
 * 前端用 antd Table 渲染。多 sheet 用 Tabs 切换。
 *
 * 截断说明：单 sheet 超过 ~30k 字符时，后端取头 40 行 + 尾 10 行（中间插占位行），
 * 在 Tab 标签上用 `已截断` Tag 提示。
 */
export function XlsxPreview({ rel }: Props) {
  const [data, setData] = useState<ExcelPreviewData | null>(null);
  const [error, setError] = useState<string | null>(null);
  const [loading, setLoading] = useState(true);

  useEffect(() => {
    let cancelled = false;
    setLoading(true);
    setError(null);
    setData(null);

    attachmentApi
      .previewExcel(rel)
      .then((d) => {
        if (!cancelled) setData(d);
      })
      .catch((e) => {
        if (!cancelled) setError(String(e));
      })
      .finally(() => {
        if (!cancelled) setLoading(false);
      });

    return () => {
      cancelled = true;
    };
  }, [rel]);

  if (loading) {
    return (
      <div
        className="flex items-center justify-center"
        style={{ height: "100%", minHeight: 240 }}
      >
        <Spin tip="正在解析 Excel..." />
      </div>
    );
  }

  if (error) {
    return (
      <Alert
        type="error"
        showIcon
        message="Excel 预览失败"
        description={error}
      />
    );
  }

  if (!data || data.sheets.length === 0) {
    return <Alert type="info" message="文件没有任何 Sheet" />;
  }

  return (
    <Tabs
      size="small"
      items={data.sheets.map((sheet, sheetIdx) => ({
        key: String(sheetIdx),
        label: (
          <span>
            {sheet.name}
            {sheet.truncatedRows > 0 && (
              <Tag color="orange" style={{ marginLeft: 6, fontSize: 10 }}>
                已截断
              </Tag>
            )}
          </span>
        ),
        children: <SheetTable sheet={sheet} />,
      }))}
    />
  );
}

interface SheetTableProps {
  sheet: ExcelPreviewData["sheets"][number];
}

/**
 * 单 sheet 渲染。
 *
 * 用 antd Table 而不是裸 <table>：
 * - 内建虚拟滚动（大表友好）
 * - 列宽自动 + 横向滚动
 * - 单元格自动 ellipsis 防止超宽
 */
function SheetTable({ sheet }: SheetTableProps) {
  const columns = useMemo(() => {
    if (sheet.headers.length === 0) {
      // 没表头：用第一行长度生成 col1/col2/...
      const w = sheet.rows[0]?.length ?? 0;
      return Array.from({ length: w }, (_, i) => ({
        title: `col${i + 1}`,
        dataIndex: String(i),
        key: String(i),
        ellipsis: true,
        width: 140,
      }));
    }
    return sheet.headers.map((h, i) => ({
      title: h || `col${i + 1}`,
      dataIndex: String(i),
      key: String(i),
      ellipsis: true,
      width: 140,
    }));
  }, [sheet.headers, sheet.rows]);

  const dataSource = useMemo(
    () =>
      sheet.rows.map((row, rowIdx) => {
        const o: Record<string, string> = { __key: String(rowIdx) };
        row.forEach((cell, i) => {
          o[String(i)] = cell;
        });
        return o;
      }),
    [sheet.rows],
  );

  if (sheet.rows.length === 0 && sheet.headers.length === 0) {
    return <Alert type="info" message="（空 Sheet）" showIcon />;
  }

  return (
    <div>
      <div style={{ fontSize: 12, color: "#999", marginBottom: 6 }}>
        共 {sheet.totalRows} 行
        {sheet.truncatedRows > 0 && (
          <span style={{ marginLeft: 8, color: "#d4811f" }}>
            （已省略中间 {sheet.truncatedRows} 行）
          </span>
        )}
      </div>
      <Table
        columns={columns}
        dataSource={dataSource}
        rowKey="__key"
        size="small"
        bordered
        scroll={{ x: "max-content", y: 480 }}
        pagination={{
          pageSize: 50,
          size: "small",
          showSizeChanger: false,
          hideOnSinglePage: true,
        }}
      />
    </div>
  );
}
