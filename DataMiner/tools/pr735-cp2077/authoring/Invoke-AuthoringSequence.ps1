param(
    [Parameter(Mandatory=$true)][string]$RunDirectory,
    [Parameter(Mandatory=$true)][ValidateSet('authoring-a','authoring-b')][string]$Stage,
    [switch]$FovOnly
)
$ErrorActionPreference='Stop'
# The automatic sequence depended on GPU prompts that have been withdrawn after
# an unexplained game exit at arm. Reject before loading a bundle or publishing
# any control files. Sealed historical bundles must not be reused.
throw 'CP2077_AUTHORING_SEQUENCE_DISABLED: GPU operator prompts are withdrawn. Do not launch or arm through this sequence. Use separately confirmed Capture-AuthoringWindow.ps1 phases only after offline validation and a new tool seal; never reuse a failed session.'
