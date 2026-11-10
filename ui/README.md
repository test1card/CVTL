# CVTL GUI M0

Четыре автономные сборки сравнивают Svelte/React и uPlot/ECharts на общей синтетической схеме и 1200 точках. Никаких соединений с приборами и команд управления нет.

Нужен Node 22. В каталоге `ui` выполните `npm ci`, `npm run build`, `npx playwright install --with-deps chromium`, `npm test`. Тест запускает временный HTTP-сервер localhost и headless Chromium; при наличии Chrome можно указать `CHROMIUM_PATH`. Для ручного просмотра раздайте нужную папку `dist/<framework>-<chart>/` локальным HTTP-сервером. Результаты измерений и ограничения метода — в `docs/adr/0001-gui.md`.
