# Формат профилей управления

> Статус: версия 1 (этап 6). Разбор — `rustdroid_input::profile`, движок — `rustdroid_input::keymap`.

## Принципы

- JSON с полем `version` (целое) и цепочкой миграций `vN → vN+1`. Старые профили открываются
  всегда, новые поля имеют значения по умолчанию.
- Координаты — в **долях экрана** (0–1), `(0, 0)` — левый верхний угол. Радиусы и шаг щипка —
  в долях **высоты** экрана: круг остаётся кругом на широком экране.
- Профиль привязан к `package` и автоматически включается, когда guest-agent сообщает, что
  игра на переднем плане.
- Клавиши — по именам (регистр не важен): буквы `A`…`Z`, цифры `0`…`9`, `F1`…`F12`, `Space`,
  `Enter`, `Escape`, `Tab`, `Backspace`, `CapsLock`, `LShift`/`RShift`, `LCtrl`/`RCtrl`,
  `LAlt`/`RAlt`, `Up`/`Down`/`Left`/`Right`, `Home`, `End`, `PageUp`, `PageDown`, `Insert`,
  `Delete`, `Minus`, `Equals`, `LBracket`, `RBracket`, `Semicolon`, `Apostrophe`, `Grave`,
  `Backslash`, `Comma`, `Period`, `Slash`, `Num0`…`Num9`; мышь — `LButton`, `RButton`,
  `MButton`, колесо — `Wheel` (полный список — `rustdroid_input::keys::key_name`).
- Клавиша, занятая профилем, в гостя не передаётся; остальные работают как обычно (Esc —
  «Назад», F2 — «Домой», F3 — «Недавние»). Левая кнопка без прицела — обычное касание мышью.

## Набросок

```json
{
  "version": 1,
  "name": "Example Shooter — по умолчанию",
  "package": "com.example.shooter",
  "orientation": "landscape",
  "controls": [
    { "type": "tap", "key": "F", "x": 0.82, "y": 0.64 },
    { "type": "joystick", "keys": { "up": "W", "left": "A", "down": "S", "right": "D" },
      "x": 0.15, "y": 0.72, "radius": 0.08 },
    { "type": "aim", "toggle_key": "LAlt", "x": 0.65, "y": 0.45,
      "sensitivity": { "x": 1.0, "y": 1.0 }, "invert_y": false },
    { "type": "shoot", "key": "LButton", "x": 0.88, "y": 0.52 },
    { "type": "swipe", "key": "Space", "from": { "x": 0.5, "y": 0.8 }, "to": { "x": 0.5, "y": 0.4 }, "duration_ms": 80 },
    { "type": "repeat_tap", "key": "E", "x": 0.7, "y": 0.8, "interval_ms": 50 },
    { "type": "hold", "key": "C", "x": 0.9, "y": 0.85, "mode": "toggle" },
    { "type": "zoom", "key": "Wheel", "center": { "x": 0.5, "y": 0.5 }, "step": 0.05 },
    { "type": "macro", "key": "Q", "steps": [
        { "tap": { "x": 0.6, "y": 0.6 } }, { "wait_ms": 120 }, { "tap": { "x": 0.7, "y": 0.6 } } ] },
    { "type": "moba_skill", "key": "1", "x": 0.78, "y": 0.82, "radius": 0.07,
      "cancel_key": "Escape", "cancel": { "x": 0.9, "y": 0.2 } }
  ]
}
```

## Типы контролов

| Тип | Поведение |
|---|---|
| `tap` | касание в точке, пока нажата клавиша |
| `joystick` | клавиши направлений → палец в центре стика, сдвинутый на `radius` в сторону нажатых |
| `aim` | `toggle_key` включает прицел: курсор скрыт и заперт в окне, движение мыши тянет палец от `x, y` (чувствительность X/Y, инверсия); палец дальше 0,2 высоты от старта ставится заново |
| `shoot` | касание, пока нажата клавиша, — только при включённом прицеле |
| `swipe` | свайп от `from` к `to` за `duration_ms` (по умолчанию 80) |
| `repeat_tap` | касания с интервалом `interval_ms` (по умолчанию 50), пока нажата клавиша |
| `hold` | удержание (`hold`) или переключение (`toggle`) |
| `zoom` | колесо → щипок двумя пальцами вокруг `center`, `step` на щелчок (по умолчанию 0,05) |
| `macro` | шаги `{"tap": {x, y}}` (касание 30 мс) и `{"wait_ms": N}` |
| `moba_skill` | нажатие → палец на скилле, курсор относительно центра экрана задаёт направление (полный `radius` — от 0,3 высоты), отпускание → применение; `cancel_key` уводит палец в `cancel` и отпускает |
| геймпад | кнопки XInput — клавиши `PadA`, `PadB`, `PadX`, `PadY`, `PadLB`, `PadRB`, `PadLT`, `PadRT`, `PadStart`, `PadBack`, `PadLS`, `PadRS`, крестовина `PadUp`/`PadDown`/`PadLeft`/`PadRight` — на любой контрол; поле `"stick": "PadLeftStick"` у `joystick` — плавное управление стиком, `"stick": "PadRightStick"` у `aim` — прицел правым стиком без захвата мыши (мёртвая зона 15%) |

Одновременно — до 8 пальцев профиля (слоты 2–9 сенсорной панели; 0 — мышь, 1 — колесо).

## Включение

- `rustdroid boot-android … --profile game.rdprofile` — профиль включён всегда;
- `--profiles DIR` — все `*.rdprofile` из папки: профиль включается сам, когда его `package` на
  переднем плане (по данным гостевого агента, `--agent`); профиль без `package` — общий,
  когда для игры нет своего. При смене профиля пальцы старого отпускаются.

Экспорт и импорт — файл `.rdprofile` (тот же JSON).
