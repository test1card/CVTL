# Криогенные свойства материалов

`nist.csv` содержит опубликованные коэффициенты аппроксимаций NIST. Температура задана в К, удельная теплоёмкость в Дж/(кг·К), теплопроводность в Вт/(м·К). При загрузке формула вычисляется на сетке 0,01 К. Проверяются положительность значений и строгое возрастание интеграла. Кусочно-линейная кривая интегрируется аналитически. При сравнении с формулами NIST в серединах интервалов сетки наибольшее относительное расхождение составило около 9,6·10⁻⁶ для теплопроводности стали 304 вблизи нижней границы. Это результат численной проверки в заданных точках.

Для каждого свойства в CSV записаны диапазон применимости уравнения и отклонение аппроксимации от исходных данных. Неопределённость конкретного образца оценивается отдельно. Для теплопроводности 6061-T6 и стали 304 исходные данные охватывают 4–300 К, а NIST публикует уравнения для 1–300 К. Для теплопроводности OFHC выбран профиль `RRR=100`. У G-10 CR теплопроводность выбирают для направления `normal` или `warp`, теплоёмкость доступна по `g10_cr`.

Для Al 1100 доступна теплопроводность. Криогенная зависимость теплоёмкости для марки UNS A91100 остаётся `unverified`. Запрос возвращает `UnavailableProperty`.

Карточки NIST — [OFHC](https://trc.nist.gov/cryogenics/materials/OFHC%20Copper/OFHC_Copper_rev1.htm), [6061-T6](https://trc.nist.gov/cryogenics/materials/6061%20Aluminum/6061_T6Aluminum_rev.htm), [1100](https://trc.nist.gov/cryogenics/materials/1100%20Aluminum/1100%20Aluminum_rev.htm), [304](https://trc.nist.gov/cryogenics/materials/304Stainless/304Stainless_rev.htm), [G-10 CR](https://trc.nist.gov/cryogenics/materials/G-10%20CR%20Fiberglass%20Epoxy/G10CRFiberglassEpoxy_rev.htm).

## Проводка Lake Shore

`lakeshore-wire.csv` содержит точки `k(T)` из таблицы производителя. Пустая ячейка означает отсутствие значения. CLI принимает `lakeshore_copper`, `lakeshore_phosphor_bronze`, `lakeshore_nichrome` и `lakeshore_manganin`. Между опубликованными точками используется линейная интерполяция. [Характеристики проводов Lake Shore](https://www.lakeshore.com/products/categories/specification/temperature-products/cryogenic-accessories/cryogenic-wire)

Каталог производителя связывает WCT-RB-34-50 с посеребрённой медью 34 AWG, WQT-36-100 с фосфористой бронзой 36 AWG. Для CT-34 и QT-36 опубликовано сопротивление на метр при 4,2, 77 и 305 К. Для перевода этих данных в `ρ(T)` нужно умножить сопротивление на метр на площадь соответствующей жилы и указать источник таблицы во входе CLI. Сечение 0,12 мм² из медного эталона задаётся условием расчёта. Для CT-34 табличный диаметр жилы 0,160 мм даёт около 0,0201 мм². Сопротивление МГТФ и конкретных образцов требуется измерить или подтвердить по их данным. [Каталог Lake Shore, с. 139–140](https://www.lakeshore.com/docs/default-source/product-downloads/lakeshoretc_l.pdf), [таблица сопротивлений Lake Shore](https://www.lakeshore.com/products/categories/specification/temperature-products/cryogenic-accessories/cryogenic-wire)
