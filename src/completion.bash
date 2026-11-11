# Tab completion for mynou in bash. Load it from ~/.bash_profile with:
#   eval "$(mynou completion --shell bash)"

_mynou() {
  local IFS=$'\n' cur=${COMP_WORDS[COMP_CWORD]} prev=${COMP_WORDS[COMP_CWORD-1]}
  COMPREPLY=()
  if [ "$COMP_CWORD" -eq 1 ]; then
    COMPREPLY=($(compgen -W $'serve\nservice\nupdate\nversion\ncompletion\nhelp' -- "$cur"))
    return
  fi
  case $prev in
    --dir) COMPREPLY=($(compgen -d -- "$cur")); return ;;
    --host) COMPREPLY=($(compgen -W $'127.0.0.1\n0.0.0.0' -- "$cur")); return ;;
    --port) return ;;
    --action) COMPREPLY=($(compgen -W $'install\nstatus\nstart\nstop\nuninstall' -- "$cur")); return ;;
    --label) COMPREPLY=($(compgen -W 'org.mynou.automation' -- "$cur")); return ;;
    --shell) COMPREPLY=($(compgen -W $'zsh\nbash' -- "$cur")); return ;;
  esac
  local options word given i
  case ${COMP_WORDS[1]} in
    serve) options=$'--dir\n--host\n--port\n--help' ;;
    service) options=$'--action\n--dir\n--label\n--help' ;;
    update) options=$'--dir\n--help' ;;
    completion) options=$'--shell\n--help' ;;
    *) return ;;
  esac
  # Each option is accepted once.
  for ((i = 2; i < COMP_CWORD; i++)); do
    given=${COMP_WORDS[i]}
    options=$(printf '%s\n' $options | grep -vxF -- "$given")
  done
  COMPREPLY=($(compgen -W "$options" -- "$cur"))
}
complete -o filenames -F _mynou mynou
