import React, { useEffect, useRef, useState } from "react";
import { createRoot } from "react-dom/client";
import { chart } from "@chart";
import { svg, title } from "./common";
function App() {
  const [mode, setMode] = useState("temperature");
  const target = useRef(null),
    plot = useRef(null);
  useEffect(() => {
    plot.current = chart(target.current, "temperature");
    return () => plot.current.destroy();
  }, []);
  function change(m) {
    setMode(m);
    plot.current.update(m);
  }
  return (
    <main>
      <header>
        <strong>CVTL</strong>
        <span>● Симулятор · данные синтетические</span>
      </header>
      <h1>Ход опыта</h1>
      <p>
        Прототип сравнения интерфейсов M0. Управление оборудованием отсутствует.
      </p>
      <section>
        <h2>Тепловая схема</h2>
        <div dangerouslySetInnerHTML={{ __html: svg }} />
      </section>
      <section>
        <div className="bar">
          <h2>{title(mode)}</h2>
          <div>
            <button
              aria-pressed={mode === "temperature"}
              onClick={() => change("temperature")}
            >
              Температура
            </button>
            <button
              aria-pressed={mode === "power"}
              onClick={() => change("power")}
            >
              Мощность
            </button>
          </div>
        </div>
        <div ref={target} className="plot" />
      </section>
    </main>
  );
}
export function mountApp(target) {
  createRoot(target).render(<App />);
}
