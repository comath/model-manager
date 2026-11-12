import type { ModelKind } from "../types";
import { KINDS, KIND_LABEL } from "../types";

export function KindPicker({
  value,
  onChange,
}: {
  value: ModelKind;
  onChange: (kind: ModelKind) => void;
}) {
  return (
    <div className="kind-picker" role="radiogroup" aria-label="Download type">
      {KINDS.map((kind) => (
        <button
          key={kind}
          type="button"
          role="radio"
          aria-checked={value === kind}
          className={value === kind ? "active" : ""}
          onClick={() => onChange(kind)}
        >
          {KIND_LABEL[kind]}
        </button>
      ))}
    </div>
  );
}
