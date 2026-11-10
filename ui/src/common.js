export const series = Array.from({ length: 1200 }, (_, i) => [
  i,
  80 - 35 * (1 - Math.exp(-i / 450)) + 0.3 * Math.sin(i / 20),
  38 + 2 * Math.sin(i / 110),
]);
export function data(mode) {
  return [
    series.map((x) => x[0]),
    series.map((x) => x[mode === "temperature" ? 1 : 2]),
  ];
}
export const svg = `<svg viewBox="0 0 470 150" role="img" aria-label="Схема виртуального тела и второй ступени"><rect x="20" y="35" width="130" height="75" rx="12" fill="#d7e8f1" stroke="#11678c"/><text x="85" y="65" text-anchor="middle">Тело</text><text x="85" y="88" text-anchor="middle">45,2 К</text><path d="M150 72 H310 M300 65 L310 72 L300 79" fill="none" stroke="#11678c" stroke-width="4"/><rect x="310" y="35" width="140" height="75" rx="12" fill="#d7e8f1" stroke="#11678c"/><text x="380" y="65" text-anchor="middle">Ступень 2</text><text x="380" y="88" text-anchor="middle">38,1 К</text></svg>`;
export const title = (mode) =>
  mode === "temperature" ? "Температура, К" : "Мощность, Вт";
