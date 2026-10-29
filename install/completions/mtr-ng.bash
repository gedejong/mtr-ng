_mtr__ng() {
    local i cur prev opts cmd
    COMPREPLY=()
    if [[ "${BASH_VERSINFO[0]}" -ge 4 ]]; then
        cur="$2"
    else
        cur="${COMP_WORDS[COMP_CWORD]}"
    fi
    prev="$3"
    cmd=""
    opts=""

    for i in "${COMP_WORDS[@]:0:COMP_CWORD}"
    do
        case "${cmd},${i}" in
            ",$1")
                cmd="mtr__ng"
                ;;
            *)
                ;;
        esac
    done

    case "${cmd}" in
        mtr__ng)
            opts="-c -i -M -4 -6 -r -j -C -n -z -g -e -P -q -h -V --count --interval --max-hops --report --json --csv --dot --mermaid --geojson --numeric --aslookup --geo --geoip-mmdb --privacy-hops --origin --mpls --flows --pmtu --sparkline-scale --ema-alpha --fields --show-all --simulate --protocol --metrics --record --replay --replay-speed --quiet --log-file --help --version"
            if [[ ${cur} == -* || ${COMP_CWORD} -eq 1 ]] ; then
                COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
                return 0
            fi
            case "${prev}" in
                --count)
                    COMPREPLY=($(compgen -f "${cur}"))
                    return 0
                    ;;
                -c)
                    COMPREPLY=($(compgen -f "${cur}"))
                    return 0
                    ;;
                --interval)
                    COMPREPLY=($(compgen -f "${cur}"))
                    return 0
                    ;;
                -i)
                    COMPREPLY=($(compgen -f "${cur}"))
                    return 0
                    ;;
                --max-hops)
                    COMPREPLY=($(compgen -f "${cur}"))
                    return 0
                    ;;
                -M)
                    COMPREPLY=($(compgen -f "${cur}"))
                    return 0
                    ;;
                --geoip-mmdb)
                    COMPREPLY=($(compgen -f "${cur}"))
                    return 0
                    ;;
                --privacy-hops)
                    COMPREPLY=($(compgen -f "${cur}"))
                    return 0
                    ;;
                --origin)
                    COMPREPLY=($(compgen -f "${cur}"))
                    return 0
                    ;;
                --flows)
                    COMPREPLY=($(compgen -f "${cur}"))
                    return 0
                    ;;
                --sparkline-scale)
                    COMPREPLY=($(compgen -W "linear logarithmic" -- "${cur}"))
                    return 0
                    ;;
                --ema-alpha)
                    COMPREPLY=($(compgen -f "${cur}"))
                    return 0
                    ;;
                --fields)
                    COMPREPLY=($(compgen -W "hop host asn as-name prefix geo dist stretch reach back loss sent last avg ema jitter jitter-avg best worst stdev graph" -- "${cur}"))
                    return 0
                    ;;
                --protocol)
                    COMPREPLY=($(compgen -W "icmp udp tcp" -- "${cur}"))
                    return 0
                    ;;
                -P)
                    COMPREPLY=($(compgen -W "icmp udp tcp" -- "${cur}"))
                    return 0
                    ;;
                --metrics)
                    COMPREPLY=($(compgen -f "${cur}"))
                    return 0
                    ;;
                --record)
                    COMPREPLY=($(compgen -f "${cur}"))
                    return 0
                    ;;
                --replay)
                    COMPREPLY=($(compgen -f "${cur}"))
                    return 0
                    ;;
                --replay-speed)
                    COMPREPLY=($(compgen -f "${cur}"))
                    return 0
                    ;;
                --log-file)
                    COMPREPLY=($(compgen -f "${cur}"))
                    return 0
                    ;;
                *)
                    COMPREPLY=()
                    ;;
            esac
            COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
            return 0
            ;;
    esac
}

if [[ "${BASH_VERSINFO[0]}" -eq 4 && "${BASH_VERSINFO[1]}" -ge 4 || "${BASH_VERSINFO[0]}" -gt 4 ]]; then
    complete -F _mtr__ng -o nosort -o bashdefault -o default mtr-ng
else
    complete -F _mtr__ng -o bashdefault -o default mtr-ng
fi
