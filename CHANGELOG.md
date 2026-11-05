# Alterações

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
