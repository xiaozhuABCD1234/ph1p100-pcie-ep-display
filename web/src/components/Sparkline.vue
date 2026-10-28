<script setup lang="ts">
import { onBeforeUnmount, onMounted, ref, watch } from 'vue'

const props = withDefaults(
  defineProps<{ values: number[]; color?: string; max?: number }>(),
  { color: '#35e0d0', max: 60 },
)

const canvas = ref<HTMLCanvasElement | null>(null)
let observer: ResizeObserver | null = null

function draw() {
  const el = canvas.value
  if (!el) return
  const dpr = window.devicePixelRatio || 1
  const w = Math.max(1, el.clientWidth)
  const h = Math.max(1, el.clientHeight)
  if (el.width !== Math.round(w * dpr) || el.height !== Math.round(h * dpr)) {
    el.width = Math.round(w * dpr)
    el.height = Math.round(h * dpr)
  }
  const ctx = el.getContext('2d')
  if (!ctx) return
  ctx.setTransform(dpr, 0, 0, dpr, 0, 0)
  ctx.clearRect(0, 0, w, h)

  // 背景网格：三条水平线，弱到只作节奏参考
  ctx.strokeStyle = 'rgba(148, 178, 220, 0.12)'
  ctx.lineWidth = 1
  for (let i = 1; i <= 3; i += 1) {
    const y = (h / 4) * i + 0.5
    ctx.beginPath()
    ctx.moveTo(0, y)
    ctx.lineTo(w, y)
    ctx.stroke()
  }

  const values = props.values
  if (values.length < 2) return
  const top = Math.max(props.max, ...values, 1)
  const pad = 4
  const x = (i: number) => (i / (values.length - 1)) * w
  const y = (v: number) => h - pad - (Math.min(v, top) / top) * (h - pad * 2)

  const area = ctx.createLinearGradient(0, 0, 0, h)
  area.addColorStop(0, `${props.color}44`)
  area.addColorStop(1, `${props.color}00`)
  ctx.beginPath()
  ctx.moveTo(0, h)
  values.forEach((v, i) => ctx.lineTo(x(i), y(v)))
  ctx.lineTo(w, h)
  ctx.closePath()
  ctx.fillStyle = area
  ctx.fill()

  ctx.beginPath()
  values.forEach((v, i) => {
    if (i === 0) ctx.moveTo(x(i), y(v))
    else ctx.lineTo(x(i), y(v))
  })
  ctx.strokeStyle = props.color
  ctx.lineWidth = 1.7
  ctx.lineJoin = 'round'
  ctx.lineCap = 'round'
  ctx.shadowColor = props.color
  ctx.shadowBlur = 6
  ctx.stroke()
  ctx.shadowBlur = 0

  const lastX = x(values.length - 1)
  const lastY = y(values[values.length - 1])
  ctx.beginPath()
  ctx.arc(lastX, lastY, 2.2, 0, Math.PI * 2)
  ctx.fillStyle = props.color
  ctx.fill()
}

watch(() => props.values, draw, { deep: true })
onMounted(() => {
  draw()
  observer = new ResizeObserver(draw)
  if (canvas.value) observer.observe(canvas.value)
})
onBeforeUnmount(() => observer?.disconnect())
</script>

<template>
  <canvas ref="canvas" class="sparkline" />
</template>
