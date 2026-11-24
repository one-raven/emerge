defmodule Emerge.UI.Lazy do
  @moduledoc """
  One Raven patch: memoized subtrees for `Emerge.UI.lazy/3`.

  A lazy subtree is tagged `{id, deps}`. Two things make an unchanged one
  free on a rerender:

    * Inside a render frame (`frame/1`; `Emerge.Runtime.Viewport` wraps every
      `render` in one), the subtree built for `id` on the previous frame is
      returned as it is when `deps` are equal, so the builder function isn't
      called. Entries not used in a frame are dropped at its end, so the cache
      holds one frame's lazy subtrees.
    * The differ (`Emerge.Engine.Reconcile`) reuses the previous subtree when
      the node it matches carries an equal tag, without walking it. This holds
      outside a frame too.

  `deps` must cover everything the function reads: the subtree is assumed to
  be a pure function of them. Compare with `===`, so `1` and `1.0` differ.
  """

  alias Emerge.Engine.Element

  @cur {__MODULE__, :cur}
  @prev {__MODULE__, :prev}

  @doc false
  @spec lazy(term(), term(), (-> Element.t())) :: Element.t()
  def lazy(id, deps, fun) when is_function(fun, 0) do
    case Process.get(@cur) do
      nil ->
        build(id, deps, fun)

      cur ->
        case cur do
          %{^id => {^deps, element}} ->
            element

          _ ->
            element =
              case Process.get(@prev) do
                %{^id => {^deps, element}} -> element
                _ -> build(id, deps, fun)
              end

            # Re-read: `fun` may have run lazy subtrees of its own.
            Process.put(@cur, Map.put(Process.get(@cur), id, {deps, element}))
            element
        end
    end
  end

  @doc """
  Run `fun` (a whole render) as one frame of the lazy cache. Nested calls run
  inside the outer frame.
  """
  @spec frame((-> result)) :: result when result: term()
  def frame(fun) when is_function(fun, 0) do
    if Process.get(@cur) do
      fun.()
    else
      Process.put(@cur, %{})

      try do
        result = fun.()
        Process.put(@prev, Process.get(@cur))
        result
      after
        Process.delete(@cur)
      end
    end
  end

  @doc "Drop the cache (both frames)."
  @spec clear() :: :ok
  def clear do
    Process.delete(@cur)
    Process.delete(@prev)
    :ok
  end

  defp build(id, deps, fun) do
    case fun.() do
      %Element{} = element ->
        %{element | memo: {id, deps}}

      other ->
        raise ArgumentError,
              "lazy/3 expects its function to return an Emerge element, got: #{inspect(other)}"
    end
  end
end
