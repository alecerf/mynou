#compdef mynou
# Tab completion for mynou in zsh. Load it from ~/.zshrc with:
#   eval "$(mynou completion --shell zsh)"

_mynou() {
  local curcontext=$curcontext state line
  local -a commands
  commands=(
    'serve:start Mynou and its web interface'
    'service:start Mynou at login with launchd'
    'update:install the latest release now'
    'version:show the version'
    'completion:print this completion script'
    'help:show the help'
  )
  _arguments -C '1: :->command' '*:: :->option' && return
  case $state in
    command)
      _describe -t commands 'mynou command' commands
      ;;
    option)
      case $words[1] in
        serve)
          _arguments \
            '--dir[data directory]:directory:_directories' \
            '--host[address to listen on for this run]:host:(127.0.0.1 0.0.0.0)' \
            '--port[port to listen on for this run]:port:' \
            '--help[show the help]'
          ;;
        service)
          _arguments \
            '--action[what to do with the service]:action:(install status start stop uninstall)' \
            '--dir[absolute data directory]:directory:_directories' \
            '--label[launchd label]:label:(org.mynou.automation)' \
            '--help[show the help]'
          ;;
        update)
          _arguments \
            '--dir[data directory]:directory:_directories' \
            '--help[show the help]'
          ;;
        completion)
          _arguments \
            '--shell[shell to complete in]:shell:(zsh bash)' \
            '--help[show the help]'
          ;;
      esac
      ;;
  esac
}

if [[ $funcstack[1] == _mynou ]]; then
  # Autoloaded from a directory on $fpath.
  _mynou "$@"
else
  (( $+functions[compdef] )) || { autoload -Uz compinit && compinit; }
  compdef _mynou mynou
fi
