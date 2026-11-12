# Патчи к gfxstream

Применяются по порядку имён в workflow `.github/workflows/gfxstream.yml` перед сборкой
зафиксированного коммита `google/gfxstream`.

| Патч | Зачем |
|---|---|
| `0001-vk-getReason-c_str.patch` | `std::string` передавался в `GFXSTREAM_INFO` (printf-подобный) без `.c_str()` в ветке кода только для Windows; Clang 22 это запрещает |
| `0002-compositor-descriptor-pool-size.patch` | Пул дескрипторов Vulkan-композитора не учитывал, что наборы для immediate-отрисовки выделяются на каждый кадр в полёте (их 3): при числе форматов меньше трёх пула не хватало. NVIDIA, AMD и Mesa это прощают, драйвер Intel под Windows возвращает `VK_ERROR_OUT_OF_POOL_MEMORY`, и gfxstream завершает процесс (`Failed to create Vulkan compositor`). Найдено на Intel UHD (i3-10110U, драйвер 27.20.100.8984) |
| `0003-dispatch-core-1-3-aliases.patch` | Драйвер Vulkan 1.2 (Intel UHD 27.20.100.8984) даёт функции, вошедшие в ядро 1.3, только под именами расширений (`vkQueueSubmit2KHR`, `vkCmdPipelineBarrier2KHR`, `vkCmdBeginRenderingKHR`, `vkCmdSetCullModeEXT`…), а декодер gfxstream вызывает их по именам ядра — указатель пустой, и процесс падает по адресу 0 (`dispatchVkQueueSubmit` → `vkQueueSubmit2`). Патч подставляет функцию расширения, если функции ядра нет (36 функций, сигнатуры совпадают) |
| `0004-queue-submit2-fallback.patch` | У драйвера Intel UHD 27.20.100.8984 нет VK_KHR_synchronization2: нет ни `vkQueueSubmit2`, ни `vkQueueSubmit2KHR`, и патча 0003 мало. Гостевой драйвер же всегда отправляет `VkSubmitInfo2`. Если `vkQueueSubmit2` нет, патч переводит `VkSubmitInfo2` в `VkSubmitInfo` (+ `VkTimelineSemaphoreSubmitInfo`, `VkProtectedSubmitInfo`) и вызывает `vkQueueSubmit`; стадии `VkPipelineStageFlags2` за пределами 32 бит заменяются на `ALL_COMMANDS` |
| `0005-sync2-command-shims.patch` | То же для команд в буферах команд: если у драйвера нет synchronization2, `vkCmdPipelineBarrier2`, `vkCmdWaitEvents2`, `vkCmdSetEvent2`, `vkCmdResetEvent2`, `vkCmdWriteTimestamp2` (и их `…KHR`) заменяются переходниками на команды Vulkan 1.0: барьеры объединяются, стадии сливаются в одну маску, флаги доступа за пределами 32 бит сводятся к `SHADER_READ`/`SHADER_WRITE`/`MEMORY_*`. В лог gfxstream пишется строка `RustDroid: no synchronization2 in the driver` |
