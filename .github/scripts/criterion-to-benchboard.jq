def median: sort | (.[(length - 1) / 2 | floor] + .[length / 2 | floor]) / 2;

[.[] | to_entries[] |
  .key as $name | .value.latency.samples as $samples |
  if $program == "functions" then
    {id: "\($architecture)/\($name)", samples: $samples}
  else
    ($name | split("/")) as $parts |
    ($parts[2] | ltrimstr("-")) as $level |
    ($parts[4:] | join("/") | sub("\\.[^./]+$"; "")) as $source |
    ($parts[3] + if $source == "" then "" else "/" + $source end) as $phase |
    {id: "\($architecture)/\($parts[0])/\($program)/source/\($level)/\($phase)",
     samples: $samples,
     variant: {
       benchmark: ("\($program)/\($level)" + if $source == "" then "" else "/" + $source end),
       group: (({compile: "Compile", link: "Link", run: "Run"}[$parts[3]]) + " on " + $architecture),
       variant: $parts[0], subject: ($parts[0] == "fcc")}}
  end] |
if length == 0 or (map(.id) | unique | length) != length then
  error("missing or duplicate benchmark measurements")
else
  {metrics: [{key: "latency", label: "Wall time", unit: "ns"}],
   environment: $environment,
   results: (map({key: .id, value: {latency: {value: (.samples | median), samples: .samples}}}) | from_entries),
   variants: (map(select(.variant != null) | {key: .id, value: .variant}) | from_entries)}
end
