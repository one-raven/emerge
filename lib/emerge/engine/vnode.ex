defmodule Emerge.Engine.VNode do
  @moduledoc """
  Internal virtual node that keeps identity and keys for reconciliation.
  """

  @type t :: %__MODULE__{
          id: non_neg_integer(),
          kind: atom(),
          key: term() | nil,
          attrs: map(),
          events: %{term() => {pid(), term()}},
          children: [t()],
          nearby: [{atom(), t()}],
          memo: {term(), [{term(), non_neg_integer()}], boolean()} | nil
        }

  # One Raven patch: `memo` is `{tag, keys, reusable?}` for a subtree built by
  # `Emerge.UI.lazy/3` (see `Emerge.Engine.Reconcile`).
  defstruct [:id, :kind, :key, :attrs, events: %{}, children: [], nearby: [], memo: nil]
end
