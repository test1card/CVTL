import uPlot from "uplot";
import "uplot/dist/uPlot.min.css";
import { data, title } from "./common";
export function chart(node, mode) {
  const p = new uPlot(
    {
      width: 720,
      height: 300,
      legend: { show: false },
      series: [{}, { label: title(mode), stroke: "#0072b2", width: 2 }],
      axes: [{ label: "Время, с" }, { label: title(mode) }],
    },
    data(mode),
    node,
  );
  return {
    update(m) {
      p.setData(data(m));
      p.axes[1].label = title(m);
      p.redraw();
    },
    destroy() {
      p.destroy();
    },
  };
}
