import * as echarts from "echarts";
import { data, title } from "./common";
export function chart(node, mode) {
  const p = echarts.init(node, null, { width: 720, height: 300 });
  function update(m) {
    const d = data(m);
    p.setOption({
      animation: false,
      grid: { left: 65, right: 15, top: 20, bottom: 48 },
      xAxis: { type: "value", name: "с" },
      yAxis: { type: "value", name: title(m), scale: true },
      series: [
        {
          type: "line",
          showSymbol: false,
          lineStyle: { color: "#0072b2", width: 2 },
          data: d[0].map((x, i) => [x, d[1][i]]),
        },
      ],
    });
  }
  update(mode);
  return {
    update,
    destroy() {
      p.dispose();
    },
  };
}
