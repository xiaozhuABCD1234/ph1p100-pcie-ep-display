<script setup lang="ts">
import { computed, onBeforeUnmount, onMounted, reactive, ref } from 'vue'
import CameraPane from './components/CameraPane.vue'
import Sparkline from './components/Sparkline.vue'
import Icon from './components/Icon.vue'

type Layout = 'pair' | 'a' | 'b'

const layout = ref<Layout>('pair')
const online = ref(false)
const origin = ref('')
const now = ref(new Date())
const sessionStart = Date.now()
const isFullscreen = ref(false)
const copied = ref(false)

const stats = reactive({
  pair: true,
  a_seq: 0,
  b_seq: 0,
  pair_seq: 0,
  width: 0,
  height: 0,
})

const fpsA = ref(0)
const fpsB = ref(0)
const histA = ref<number[]>([])
const histB = ref<number[]>([])

let fails = 0
let polling = false

const paneA = ref<InstanceType<typeof CameraPane> | null>(null)
const paneB = ref<InstanceType<typeof CameraPane> | null>(null)

const pad2 = (n: number) => String(n).padStart(2, '0')
const pad6 = (n: number) => String(n).padStart(6, '0')

const clock = computed(
  () => `${pad2(now.value.getHours())}:${pad2(now.value.getMinutes())}:${pad2(now.value.getSeconds())}`,
)
const dateText = computed(
  () => `${now.value.getFullYear()}-${pad2(now.value.getMonth() + 1)}-${pad2(now.value.getDate())}`,
)
const uptime = computed(() => {
  const s = Math.max(0, Math.floor((Date.now() - sessionStart) / 1000))
  const h = Math.floor(s / 3600)
  const m = Math.floor((s % 3600) / 60)
  return `${pad2(h)}:${pad2(m)}:${pad2(s % 60)}`
})
const resolution = computed(() =>
  stats.width && stats.height ? `${stats.width} × ${stats.height}` : '— × —',
)
const stageTitle = computed(() =>
  layout.value === 'pair' ? '同步双路画面' : `CAM ${layout.value.toUpperCase()} 实时画面`,
)
const pairSeq = computed(() => (stats.pair ? pad6(stats.pair_seq) : '—'))
const endpoint = computed(() => origin.value.replace(/^https?:\/\//, ''))
const activePane = computed(() => (layout.value === 'b' ? paneB.value : paneA.value))

const clampFps = (v: unknown) => Math.min(60, Math.max(0, Number(v) || 0))

async function poll() {
  if (polling) return
  polling = true
  try {
    const res = await fetch('/stats', { cache: 'no-store' })
    if (!res.ok) throw new Error(`HTTP ${res.status}`)
    const s = await res.json()
    // 帧率由服务端按 publish 间隔测量；页面只负责展示与画曲线。
    fpsA.value = clampFps(s.a_fps)
    fpsB.value = clampFps(s.b_fps)
    Object.assign(stats, {
      pair: Boolean(s.pair),
      a_seq: Number(s.a_seq) || 0,
      b_seq: Number(s.b_seq) || 0,
      pair_seq: Number(s.pair_seq) || 0,
      width: Number(s.width) || 0,
      height: Number(s.height) || 0,
    })
    if (!stats.pair && layout.value !== 'a') layout.value = 'a'
    online.value = true
    fails = 0
    histA.value = [...histA.value.slice(-59), fpsA.value]
    histB.value = [...histB.value.slice(-59), fpsB.value]
  } catch {
    fails += 1
    if (fails >= 2) {
      online.value = false
      fpsA.value = 0
      fpsB.value = 0
    }
  } finally {
    polling = false
  }
}

function downloadSnapshot(ch: 'a' | 'b') {
  const a = document.createElement('a')
  const stamp = new Date().toISOString().replace(/[:.]/g, '-')
  a.href = `/snapshot_${ch}.jpg`
  a.download = `cam-${ch}-${stamp}.jpg`
  document.body.appendChild(a)
  a.click()
  a.remove()
}

function snapshotActive() {
  downloadSnapshot(layout.value === 'b' ? 'b' : 'a')
}

async function toggleFullscreen() {
  try {
    if (document.fullscreenElement) await document.exitFullscreen()
    else await document.documentElement.requestFullscreen()
  } catch {
    /* 浏览器拒绝时保持原布局 */
  }
}

async function copyEndpoint() {
  try {
    await navigator.clipboard.writeText(origin.value)
    copied.value = true
    window.setTimeout(() => (copied.value = false), 1200)
  } catch {
    /* 非安全上下文（http 局域网）可能没有 clipboard，忽略 */
  }
}

function setLayout(next: Layout) {
  if (!stats.pair && next !== 'a') return
  layout.value = next
}

function onKey(e: KeyboardEvent) {
  const tag = (e.target as HTMLElement | null)?.tagName
  if (tag === 'INPUT' || tag === 'TEXTAREA') return
  if (e.key === '1') setLayout('pair')
  else if (e.key === '2') setLayout('a')
  else if (e.key === '3') setLayout('b')
  else if (e.key.toLowerCase() === 'f') activePane.value?.toggleFullscreen()
  else if (e.key.toLowerCase() === 's') snapshotActive()
}

function onFullscreenChange() {
  isFullscreen.value = Boolean(document.fullscreenElement)
}

let timer = 0
onMounted(() => {
  origin.value = window.location.origin
  timer = window.setInterval(() => {
    now.value = new Date()
    void poll()
  }, 1000)
  void poll()
  window.addEventListener('keydown', onKey)
  document.addEventListener('fullscreenchange', onFullscreenChange)
})
onBeforeUnmount(() => {
  window.clearInterval(timer)
  window.removeEventListener('keydown', onKey)
  document.removeEventListener('fullscreenchange', onFullscreenChange)
})
</script>

<template>
  <div class="app">
    <div class="backdrop" aria-hidden="true" />

    <header class="topbar">
      <div class="brand">
        <svg class="brand-logo" viewBox="0 0 40 40" aria-hidden="true">
          <defs>
            <linearGradient id="brandStroke" x1="4" y1="4" x2="36" y2="36">
              <stop offset="0" stop-color="#35e0d0" />
              <stop offset="1" stop-color="#4c8dff" />
            </linearGradient>
          </defs>
          <path
            d="M20 2.5 35.5 11v18L20 37.5 4.5 29V11z"
            fill="none"
            stroke="url(#brandStroke)"
            stroke-width="1.6"
          />
          <path d="M14 27V13h5.6a4.2 4.2 0 0 1 0 8.4H14" fill="none" stroke="#35e0d0" stroke-width="2" stroke-linecap="round" />
          <circle cx="25.5" cy="25.5" r="1.8" fill="#4c8dff" />
        </svg>
        <div class="brand-text">
          <h1>PH1P100 <span>视觉上位机</span></h1>
          <p>DUAL IMX415 · PCIe GEN2 ×2</p>
        </div>
      </div>

      <div class="topbar-center">
        <span class="live-pill" :class="{ off: !online }"><i />{{ online ? 'LIVE' : 'LINK LOST' }}</span>
        <span class="topbar-note">MJPEG 实时监看</span>
      </div>

      <div class="topbar-right">
        <div class="clock">
          <b>{{ clock }}</b>
          <span>{{ dateText }}</span>
        </div>
        <button class="icon-btn" type="button" title="全屏" @click="toggleFullscreen">
          <Icon :name="isFullscreen ? 'shrink' : 'expand'" :size="17" />
        </button>
      </div>
    </header>

    <main class="main">
      <section class="stage">
        <div class="stage-head">
          <div class="stage-title">
            <p class="eyebrow">REALTIME PREVIEW</p>
            <h2>{{ stageTitle }}</h2>
          </div>
          <div class="stage-tools">
            <div class="segmented" role="tablist" aria-label="画面布局">
              <button
                type="button"
                :class="{ active: layout === 'pair' }"
                :disabled="!stats.pair"
                @click="setLayout('pair')"
              >双路</button>
              <button type="button" :class="{ active: layout === 'a' }" @click="setLayout('a')">A</button>
              <button type="button" :class="{ active: layout === 'b' }" :disabled="!stats.pair" @click="setLayout('b')">B</button>
            </div>
            <button class="btn" type="button" @click="snapshotActive">
              <Icon name="camera" :size="15" />抓拍
            </button>
          </div>
        </div>

        <div class="stage-grid" :data-layout="layout">
          <CameraPane
            v-if="layout !== 'b'"
            ref="paneA"
            channel="a"
            :seq="stats.a_seq"
            :fps="fpsA"
            :width="stats.width"
            :height="stats.height"
            :available="true"
          />
          <CameraPane
            v-if="layout !== 'a'"
            ref="paneB"
            channel="b"
            :seq="stats.b_seq"
            :fps="fpsB"
            :width="stats.width"
            :height="stats.height"
            :available="stats.pair"
          />
        </div>

        <div class="stage-foot">
          <span class="foot-item"><i class="led on" />ARM_PAIR / ACK_PAIR</span>
          <span class="foot-item">PAIR {{ pairSeq }}</span>
          <span class="foot-item">{{ resolution }}</span>
          <span class="foot-spacer" />
          <span class="foot-item dim">{{ stats.pair ? '通道 A + B' : '仅通道 A' }}</span>
        </div>
      </section>

      <section class="deck">
        <section class="card">
          <div class="card-head">
            <h3>链路状态</h3>
            <span class="tag" :class="online ? 'ok' : 'bad'">{{ online ? '在线' : '离线' }}</span>
          </div>
          <dl class="kv">
            <div><dt>端点</dt><dd class="mono">{{ endpoint || '—' }}</dd></div>
            <div><dt>模式</dt><dd>{{ stats.pair ? '双路同步' : '单路' }}</dd></div>
            <div><dt>分辨率</dt><dd class="mono">{{ resolution }}</dd></div>
            <div><dt>传输</dt><dd>PCIe Gen2 ×2</dd></div>
          </dl>
        </section>

        <section class="card">
          <div class="card-head">
            <h3>帧率监视</h3>
            <span class="dim small">1 s 采样</span>
          </div>
          <div class="rate">
            <div class="rate-head">
              <span class="ch ch-a">A</span>
              <span class="rate-num">{{ fpsA.toFixed(1) }}<small>fps</small></span>
            </div>
            <Sparkline :values="histA" color="#35e0d0" />
          </div>
          <div v-if="stats.pair" class="rate">
            <div class="rate-head">
              <span class="ch ch-b">B</span>
              <span class="rate-num">{{ fpsB.toFixed(1) }}<small>fps</small></span>
            </div>
            <Sparkline :values="histB" color="#7c8cff" />
          </div>
          <p v-else class="rate-empty">B 路未启用 · 用 --source device-pair 启动</p>
        </section>

        <section class="card">
          <div class="card-head"><h3>会话</h3><span class="dim small">本次打开</span></div>
          <dl class="kv">
            <div><dt>运行时长</dt><dd class="mono">{{ uptime }}</dd></div>
            <div><dt>A 帧序号</dt><dd class="mono">{{ pad6(stats.a_seq) }}</dd></div>
            <div v-if="stats.pair"><dt>B 帧序号</dt><dd class="mono">{{ pad6(stats.b_seq) }}</dd></div>
            <div><dt>采集对</dt><dd class="mono">{{ pairSeq }}</dd></div>
          </dl>
        </section>

        <section class="card actions">
          <button class="btn ghost" type="button" @click="downloadSnapshot('a')">
            <Icon name="camera" :size="15" />抓拍 A
          </button>
          <button class="btn ghost" type="button" :disabled="!stats.pair" @click="downloadSnapshot('b')">
            <Icon name="camera" :size="15" />抓拍 B
          </button>
          <button class="btn ghost" type="button" @click="toggleFullscreen">
            <Icon name="expand" :size="15" />全屏监看
          </button>
          <button class="btn ghost" type="button" @click="copyEndpoint">
            <Icon :name="copied ? 'check' : 'copy'" :size="15" />{{ copied ? '已复制' : '复制地址' }}
          </button>
        </section>
      </section>
    </main>

    <footer class="foot">
      <span class="foot-item"><i class="led" :class="online ? 'on' : 'bad'" />{{ online ? '数据在线' : '等待服务' }}</span>
      <span class="foot-item dim">每 1 s 轮询 /stats</span>
      <span class="foot-spacer" />
      <span class="foot-item dim"><kbd>1</kbd><kbd>2</kbd><kbd>3</kbd> 布局 <em>·</em> <kbd>F</kbd> 全屏 <em>·</em> <kbd>S</kbd> 抓拍</span>
    </footer>
  </div>
</template>
