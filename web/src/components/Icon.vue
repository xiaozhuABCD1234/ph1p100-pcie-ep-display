<script setup lang="ts">
withDefaults(defineProps<{ name: string; size?: number }>(), { size: 16 })

const paths: Record<string, string> = {
  camera:
    'M4 8.5h3.1l1.5-2h6.8l1.5 2H20v10H4z M12 13.5m-3 0a3 3 0 1 0 6 0a3 3 0 1 0-6 0',
  expand: 'M9 4H4v5 M15 4h5v5 M15 20h5v-5 M9 20H4v-5',
  shrink: 'M4 9h5V4 M20 9h-5V4 M20 15h-5v5 M4 15h5v5',
  fill: 'M8 3H3v5 M16 3h5v5 M16 21h5v-5 M8 21H3v-5',
  signal: 'M4 20v-3 M9 20v-7 M14 20V9 M19 20V4',
  copy: 'M9 9h11v11H9z M5 15H4V4h11v1',
  check: 'M4 12.5 9 17.5 20 6.5',
}
</script>

<template>
  <svg
    :width="size"
    :height="size"
    viewBox="0 0 24 24"
    fill="none"
    stroke="currentColor"
    stroke-width="1.8"
    stroke-linecap="round"
    stroke-linejoin="round"
    aria-hidden="true"
  >
    <path :d="paths[name] ?? ''" />
  </svg>
</template>
