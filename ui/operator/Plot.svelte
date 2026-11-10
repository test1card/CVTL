<script lang="ts">
  import { onMount } from "svelte";
  import uPlot from "uplot";
  import "uplot/dist/uPlot.min.css";
  import type { PlotSeries, Theme } from "./types";

  let { time, series, units, label, theme, height = 240 }: {
    time: number[];
    series: PlotSeries[];
    units: string;
    label: string;
    theme: Theme;
    height?: number;
  } = $props();

  let target: HTMLDivElement;
  let width = $state(0);
  let chart: uPlot | null = null;
  let optionsKey = "";
  const hasValues = $derived(series.some((line) => line.values.some((value) => value !== null)));

  onMount(() => {
    const observer = new ResizeObserver(([entry]) => {
      width = Math.floor(entry.contentRect.width);
    });
    observer.observe(target);
    return () => {
      observer.disconnect();
      chart?.destroy();
    };
  });

  $effect(() => {
    if (width < 120) return;
    const lines = series.map(({ label, color, dash }) => ({ label, color, dash }));
    const key = JSON.stringify([label, units, theme, lines]);
    const data: uPlot.AlignedData = [time, ...series.map((line) => line.values)];
    if (!chart || optionsKey !== key) {
      const ink = theme === "dark" ? "#dce6ee" : "#344654";
      const grid = theme === "dark" ? "#344654" : "#e4eaee";
      chart?.destroy();
      chart = new uPlot({
        width,
        height,
        title: "",
        scales: { x: { time: false } },
        cursor: { drag: { x: true, y: false } },
        legend: { show: false },
        axes: [
          { label: "Время, с", stroke: ink, grid: { stroke: grid, width: 1 } },
          { label: units, stroke: ink, grid: { stroke: grid, width: 1 } },
        ],
        series: [{}, ...lines.map((line) => ({
          label: line.label,
          stroke: line.color,
          dash: line.dash,
          width: 2,
          points: { show: false },
          spanGaps: false,
        }))],
      }, data, target);
      optionsKey = key;
      return;
    }
    if (chart.width !== width || chart.height !== height) {
      chart.setSize({ width, height });
    }
    chart.setData(data);
  });
</script>

<figure class="panel chart-card" aria-label={label}>
  <figcaption>{label} <span class="chart-units">· {units}</span></figcaption>
  <ul class="chart-legend" aria-label="Ряды графика">
    {#each series as line (line.label)}
      <li>
        <svg aria-hidden="true" width="25" height="8" viewBox="0 0 25 8">
          <line x1="1" y1="4" x2="24" y2="4" stroke={line.color}
            stroke-width="2" stroke-dasharray={line.dash?.join(" ")}/>
        </svg>
        <span>{line.label}</span>
      </li>
    {/each}
  </ul>
  <div class="plot-frame" style:min-height={height + "px"}>
    <div bind:this={target} aria-hidden="true"></div>
    {#if !hasValues}<p class="plot-empty">Пока нет пригодных данных</p>{/if}
  </div>
</figure>

<style>
  .chart-card {
    margin: 0;
    min-width: 0;
  }

  figcaption {
    font-weight: 700;
    font-size: 1rem;
  }

  .chart-units,
  .plot-empty {
    color: var(--muted, #516776);
    font-weight: 400;
  }

  .chart-legend {
    display: flex;
    flex-wrap: wrap;
    gap: .35rem 1rem;
    list-style: none;
    margin: .65rem 0 .2rem;
    padding: 0;
    font-size: .8rem;
  }

  .chart-legend li {
    display: flex;
    align-items: center;
    gap: .35rem;
  }

  .plot-frame {
    position: relative;
    width: 100%;
    min-width: 0;
  }

  .plot-empty {
    position: absolute;
    inset: 0;
    display: grid;
    place-items: center;
    pointer-events: none;
    margin: 0;
    text-align: center;
    font-size: .9rem;
  }
</style>
