# Патчи к gfxstream

Применяются по порядку имён в workflow `.github/workflows/gfxstream.yml` перед сборкой
зафиксированного коммита `google/gfxstream`.

| Патч | Зачем |
|---|---|
| `0001-vk-getReason-c_str.patch` | `std::string` передавался в `GFXSTREAM_INFO` (printf-подобный) без `.c_str()` в ветке кода только для Windows; Clang 22 это запрещает |
| `0002-compositor-descriptor-pool-size.patch` | Пул дескрипторов Vulkan-композитора не учитывал, что наборы для immediate-отрисовки выделяются на каждый кадр в полёте (их 3): при числе форматов меньше трёх пула не хватало. NVIDIA, AMD и Mesa это прощают, драйвер Intel под Windows возвращает `VK_ERROR_OUT_OF_POOL_MEMORY`, и gfxstream завершает процесс (`Failed to create Vulkan compositor`). Найдено на Intel UHD (i3-10110U, драйвер 27.20.100.8984) |
| `0003-dispatch-core-1-3-aliases.patch` | Драйвер Vulkan 1.2 (Intel UHD 27.20.100.8984) даёт функции, вошедшие в ядро 1.3, только под именами расширений (`vkQueueSubmit2KHR`, `vkCmdPipelineBarrier2KHR`, `vkCmdBeginRenderingKHR`, `vkCmdSetCullModeEXT`…), а декодер gfxstream вызывает их по именам ядра — указатель пустой, и процесс падает по адресу 0 (`dispatchVkQueueSubmit` → `vkQueueSubmit2`). Патч подставляет функцию расширения, если функции ядра нет (36 функций, сигнатуры совпадают) |
