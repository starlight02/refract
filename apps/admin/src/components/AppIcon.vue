<script setup lang="ts">
/**
 * 统一的图标出口 —— Lucide（`@lucide/vue`，ISC 许可）的薄适配层。
 *
 * 集中收口的价值：
 * - 调用点用语义名（`trash` 而非 `Trash2`），不感知图标库的命名变迁；
 * - 统一几何参数：24 网格、1.5 描边、圆头圆角 —— 贴近 macOS SF Symbols
 *   的视觉语法（Lucide 默认 2 描边在 13–16px 的小尺寸下偏重）；
 * - 以后换图标库只改这一个文件。
 *
 * 具名导入是 tree-shakable 的：产物只包含下面这十几个图标。
 */
import {
  ArrowLeftRight,
  Boxes,
  Check,
  ChevronRight,
  CircleAlert,
  Copy,
  Download,
  Gauge,
  Globe,
  Info,
  Languages,
  KeyRound,
  List,
  ListChecks,
  Loader2,
  MessagesSquare,
  Moon,
  Pencil,
  Plus,
  RefreshCw,
  Search,
  Settings,
  Sparkles,
  Sun,
  Trash2,
  TriangleAlert,
  Upload,
  User,
  Users,
  Wallet,
  X,
  Zap,
} from '@lucide/vue'

const ICONS = {
  /** 仪表盘：速度表。 */
  gauge: Gauge,
  /** 渠道：双向箭头。 */
  channels: ArrowLeftRight,
  /** 日志：列表。 */
  logs: List,
  /** 密钥。 */
  key: KeyRound,
  /** 设置：齿轮。 */
  settings: Settings,
  /** 浅色主题：太阳。 */
  sun: Sun,
  /** 深色主题：月亮。 */
  moon: Moon,
  /** 新建。 */
  plus: Plus,
  /** 复制。 */
  copy: Copy,
  /** 删除：垃圾桶。 */
  trash: Trash2,
  /** 编辑：铅笔。 */
  pencil: Pencil,
  /** 测试：闪电。 */
  bolt: Zap,
  /** 导出：下载。 */
  download: Download,
  /** 语言切换。 */
  languages: Languages,
  /** 网络 / 上游。 */
  globe: Globe,
  /** AI / 推荐预设。 */
  sparkles: Sparkles,
  /** 导入：上传。 */
  upload: Upload,
  /** 成功 / 选中。 */
  check: Check,
  /** 失败 / 关闭。 */
  x: X,
  /** 重试 / 同步。 */
  refresh: RefreshCw,
  /** 批量选择：带勾清单。 */
  checklist: ListChecks,
  /** 模型总览：立体方块。 */
  boxes: Boxes,
  /** 调试台：对话气泡。 */
  chat: MessagesSquare,
  /** 搜索 / 放大镜。 */
  search: Search,
  /** 加载中 / 旋转器。 */
  spinner: Loader2,
  loader: Loader2,
  /** 右箭头 / 展开折叠。 */
  'chevron-right': ChevronRight,
  /** 警告 / 叹号。 */
  warning: TriangleAlert,
  /** 危险 / 错误圆圈。 */
  danger: CircleAlert,
  /** 提示 / 信息。 */
  info: Info,
  /** 钱包。 */
  wallet: Wallet,
  /** 个人资料。 */
  user: User,
  /** 用户管理。 */
  users: Users,
}
export type IconName = keyof typeof ICONS

withDefaults(
  defineProps<{
    name: IconName
    /** 尺寸（px）。默认 16，跟随文本场景。 */
    size?: number
  }>(),
  { size: 16 },
)
</script>

<template>
  <component :is="ICONS[name]" :size="size" :stroke-width="1.5" />
</template>
