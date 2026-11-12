# Патчи к gfxstream

Применяются по порядку имён в workflow `.github/workflows/gfxstream.yml` перед сборкой
зафиксированного коммита `google/gfxstream`.

| Патч | Зачем |
|---|---|
| `0001-vk-getReason-c_str.patch` | `std::string` передавался в `GFXSTREAM_INFO` (printf-подобный) без `.c_str()` в ветке кода только для Windows; Clang 22 это запрещает |
