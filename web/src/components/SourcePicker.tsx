import type { Source } from "../types";
import { SOURCES, SOURCE_LABEL } from "../types";

export function SourcePicker({
  value,
  onChange,
}: {
  value: Source;
  onChange: (source: Source) => void;
}) {
  return (
    <div className="kind-picker" role="radiogroup" aria-label="Dataset source">
      {SOURCES.map((source) => (
        <button
          key={source}
          type="button"
          role="radio"
          aria-checked={value === source}
          className={value === source ? "active" : ""}
          onClick={() => onChange(source)}
        >
          {SOURCE_LABEL[source]}
        </button>
      ))}
    </div>
  );
}
