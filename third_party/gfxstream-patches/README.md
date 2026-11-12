# Патчи к gfxstream

Применяются по порядку имён в workflow `.github/workflows/gfxstream.yml` перед сборкой
зафиксированного коммита `google/gfxstream`.

| Патч | Зачем |
|---|---|
| `0001-vk-getReason-c_str.patch` | `std::string` передавался в `GFXSTREAM_INFO` (printf-подобный) без `.c_str()` в ветке кода только для Windows; Clang 22 это запрещает |
| `0002-compositor-descriptor-pool-size.patch` | Пул дескрипторов Vulkan-композитора не учитывал, что наборы для immediate-отрисовки выделяются на каждый кадр в полёте (их 3): при числе форматов меньше трёх пула не хватало. NVIDIA, AMD и Mesa это прощают, драйвер Intel под Windows возвращает `VK_ERROR_OUT_OF_POOL_MEMORY`, и gfxstream завершает процесс (`Failed to create Vulkan compositor`). Найдено на Intel UHD (i3-10110U, драйвер 27.20.100.8984) |
