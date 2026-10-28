import { defineConfig } from 'vite'
import vue from '@vitejs/plugin-vue'
import { viteSingleFile } from 'vite-plugin-singlefile'

// 开发时把 /stats、/stream、/snapshot 代理到板子：
//   DISPLAY_BOARD=http://192.168.1.50:8091 pnpm dev
// 构建时打成单个 index.html，Rust 侧 include_str! 嵌进二进制，
// 板端不需要 Node，也不依赖任何 CDN 资源。
const board = process.env.DISPLAY_BOARD ?? 'http://127.0.0.1:8091'

export default defineConfig({
  plugins: [vue(), viteSingleFile()],
  server: {
    host: '0.0.0.0',
    proxy: {
      '^/(stats|stream|snapshot)': { target: board, changeOrigin: true },
    },
  },
  build: {
    target: 'es2020',
    cssCodeSplit: false,
    assetsInlineLimit: 100_000_000,
    reportCompressedSize: false,
  },
})
