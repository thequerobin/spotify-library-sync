# Alterações

## 0.3.0 — 05/11/2026

- Discografias exclusivamente dos artistas seguidos, com autorização opcional `login --discography`.
- Filas persistentes de artistas e faixas, retomada por página e orçamento global de tentativas HTTP.
- Cache por conta, mercado e filtro; álbuns compartilhados entre artistas sem importação repetida.
- Primeira listagem completa, TTL de sete dias e atualizações recentes com referência congelada.
- Lotes de 100 artistas por padrão, priorizando artistas nunca importados, e modo de descoberta sem faixas.
- Consultas offline de seguidos e lançamentos, status de cobertura e fila separada da biblioteca.
- Preservação de curtidas, álbuns salvos e catálogo de artistas não mais seguidos.
- 22 testes adicionais, incluindo 1.000 artistas, mudanças concorrentes, orçamento, permissões e transporte HTTP.

## 0.2.1 — 05/11/2026

- Importação completa retoma o offset pendente buscando referências por ID/data nas páginas próximas.
- Primeira passagem e conferência possuem checkpoints independentes.
- Mudanças detectadas na conferência preservam a importação e repetem somente a confirmação.
- Busca de referência limitada e configurável com `--resume-search-pages`.
- Compatibilidade com jobs antigos sem referências ordenadas; status exibe os dois offsets.
- Sete testes novos de retomada, offsets deslocados, limites, referências removidas e conferência interrompida.

## 0.2.0 — 05/11/2026

- `sync` passa a buscar novidades nas páginas recentes depois da primeira importação completa.
- Referência local congelada, parada após duas páginas conhecidas e conferência limitada do cabeçalho.
- Orçamentos persistidos, incluindo retries, e retomada com validação de âncoras.
- Publicação independente de curtidas e álbuns, sem remover itens ausentes de uma janela parcial.
- `--reconcile` confirma remoções; `--full` preserva a atualização ampla explícita.
- `check-saved` confirma seleções em lotes de até 40 URIs.
- Migração compatível com v0.1.0 e retomada de jobs legados.
- Cache de álbuns completos reutilizado no incremental, sem atualização ampla por TTL.
- Status de cobertura, pausas e fila; cooldown persistente também para consultas direcionadas.
- CI com testes e pacotes para Linux, Windows e macOS.
