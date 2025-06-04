# uncl

> [!WARNING]
> Early alpha release, expect glitches and bugs.

## inspiration

terminal multiplexers are overkill. tmux involves a learning curve most could do without and zellij is ... an acquired taste lets just say. what if there was something for the terminal monotasker among us, something simpler than existing solutions that makes that extra terminal tab unncessary, something that minimises context-switch cognitive load and looks cool while doing it.

## introduction

uncl is a terminal monoplexer - a single, toggleable, resizeable, and draggable floating term window as an accomplice to your worst terminal misdeeds, written in rust.

![uncl](screenshot.jpg)

## features

- toggle a floating terminal with a single `[Home]` key
- floating term is draggable and resizeable with mouse
- floating term is draggable and resizeable with keyboard
- supports most shells, tested on zsh, bash
- supports most terminal emulators, tested on wezterm
