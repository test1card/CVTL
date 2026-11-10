<script>
  import { onMount } from 'svelte';
  import { chart } from '@chart';
  import { svg, title } from './common';

  let mode = 'temperature';
  let target;
  let plot;

  onMount(() => {
    plot = chart(target, mode);
    return () => plot.destroy();
  });

  function change(next) {
    mode = next;
    plot.update(mode);
  }
</script>

<main>
  <header>
    <strong>CVTL</strong>
    <span>● Симулятор · данные синтетические</span>
  </header>
  <h1>Ход опыта</h1>
  <p>Прототип сравнения интерфейсов M0. Управление оборудованием отсутствует.</p>
  <section>
    <h2>Тепловая схема</h2>
    {@html svg}
  </section>
  <section>
    <div class="bar">
      <h2>{title(mode)}</h2>
      <div>
        <button aria-pressed={mode === 'temperature'} onclick={() => change('temperature')}>
          Температура
        </button>
        <button aria-pressed={mode === 'power'} onclick={() => change('power')}>
          Мощность
        </button>
      </div>
    </div>
    <div bind:this={target} class="plot"></div>
  </section>
</main>
