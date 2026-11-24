defmodule Emerge.Config.Scroll do
  @moduledoc """
  Scroll physics and compositor scrolling (One Raven patch). Set this record
  as `Emerge.Config.Input.scroll`. Every field can change while running.

  ```elixir
  %Emerge.Config{
    input: %Emerge.Config.Input{scroll: %Emerge.Config.Scroll{overscroll: :bounce}}
  }
  ```

  ## Fields

  - `compositor` — scroll containers move their content as a layer drawn
    from cached tiles instead of rebuilding and redrawing it every frame;
    defaults to `true`.
  - `friction` — fling friction, as Android's `ViewConfiguration`
    scroll friction; defaults to `0.015`. Higher stops sooner.
  - `min_velocity` — a release slower than this (px/s) does not fling;
    defaults to `500.0`.
  - `max_velocity` — fling velocity cap (px/s); defaults to `6000.0`.
  - `velocity_window_ms` — `nil` (default) estimates release velocity with
    a one-pole filter over pointer events; a number uses the least-squares
    slope of the pointer samples of that many milliseconds before release,
    so a finger that stops before lifting does not fling (100 is typical).
  - `touch_slop` — distance (px) a press moves before it becomes a drag;
    defaults to `10.0`.
  - `overscroll` — `:none` (default) stops at the edges; `:bounce` rubber-bands
    a drag past an edge and springs back, and a fling into an edge bounces.
    Bounce is drawn by compositor scrolling.
  """
  defstruct compositor: true,
            friction: 0.015,
            min_velocity: 500.0,
            max_velocity: 6000.0,
            velocity_window_ms: nil,
            touch_slop: 10.0,
            overscroll: :none

  @type t :: %__MODULE__{
          compositor: boolean(),
          friction: number(),
          min_velocity: number(),
          max_velocity: number(),
          velocity_window_ms: number() | nil,
          touch_slop: number(),
          overscroll: :none | :bounce
        }
  @typedoc "Typed keyword shorthand accepted in this record's parent field."
  @type options :: [
          {:compositor, boolean()}
          | {:friction, number()}
          | {:min_velocity, number()}
          | {:max_velocity, number()}
          | {:velocity_window_ms, number() | nil}
          | {:touch_slop, number()}
          | {:overscroll, :none | :bounce}
        ]
  @typedoc "Canonical form after nested keyword records have been expanded."
  @type normalized_t :: t()

  @doc false
  def __schema__,
    do: [
      compositor: :boolean,
      friction: :positive_number,
      min_velocity: :positive_number,
      max_velocity: :positive_number,
      velocity_window_ms: {:optional, :positive_number},
      touch_slop: :positive_number,
      overscroll: {:enum, [:none, :bounce]}
    ]

  @doc false
  def native(%__MODULE__{} = scroll) do
    %{
      compositor: scroll.compositor,
      friction: scroll.friction / 1,
      min_velocity: scroll.min_velocity / 1,
      max_velocity: scroll.max_velocity / 1,
      velocity_window_ms: scroll.velocity_window_ms && scroll.velocity_window_ms / 1,
      touch_slop: scroll.touch_slop / 1,
      bounce: scroll.overscroll == :bounce
    }
  end
end
