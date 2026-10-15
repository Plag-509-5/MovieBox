# API Express sur Pterodactyl

## Important : le dépôt ne contient pas de films

Ce dépôt contient le code d'un client/scraper Rust. Il ne contient et n'héberge aucun fichier vidéo. Le backend contacte les fournisseurs configurés et renvoie des métadonnées; la lecture éventuelle reste chez le fournisseur. L'API préparée ici ne propose pas de route de lecture vidéo ni de téléchargement.

## Architecture déployée

Le point d'entrée public est **Express (Node.js 22)**, adapté à un egg Node de Pterodactyl. Express démarre le binaire Rust `moviebox-api` comme processus enfant sur `127.0.0.1` et lui relaie les requêtes. Le port Rust est interne au conteneur; seul le port alloué par Pterodactyl est écouté publiquement par Express.

```text
Client → port alloué Pterodactyl → Express → 127.0.0.1:<RUST_API_PORT> → MovieBoxService Rust → fournisseurs externes
```

### Routes

| Méthode | Route | Fonction |
| --- | --- | --- |
| `GET` | `/healthz` | Santé d'Express et du backend Rust; pas d'authentification |
| `GET` | `/api/v1/providers` | Liste des fournisseurs et capacités |
| `GET` | `/api/v1/search?q=...&provider=moviebox&page=1` | Recherche (fournisseur par défaut : `moviebox`) |
| `GET` | `/api/v1/suggest?q=...` | Suggestions MovieBox |
| `GET` | `/api/v1/homepage?tab=2&page=1` | Catalogue d'accueil MovieBox |
| `GET` | `/api/v1/media/{provider}/{id}` | Détails d'un élément |

`page` va de 1 à 100; `q` est limité à 160 caractères. Les ID fournisseurs sont renvoyés par `/api/v1/providers`. Les erreurs sont au format JSON avec un code HTTP approprié.

## Préparer le paquet Pterodactyl

Le binaire Rust doit être construit sur Linux, avec la **même architecture** et une libc compatible avec l'image Node utilisée par le nœud Pterodactyl. Il faut Rust 1.90 ou plus récent et Cargo sur la machine de build — pas sur Pterodactyl.

À la racine du dépôt, sur cette machine Linux :

```sh
./deploy/pterodactyl/package.sh
```

Le script compile `moviebox-api` puis crée `dist/moviebox-api-pterodactyl.tar.gz`. Transfère cette archive dans les fichiers du serveur Pterodactyl et extrais-la à la racine (`/home/container`). Elle doit contenir :

```text
server.js
package.json
package-lock.json
bin/moviebox-api
```

Si tu compiles ailleurs que sur la machine Pterodactyl, vérifie l'architecture et la compatibilité système du binaire. Le binaire doit être exécutable (`chmod +x bin/moviebox-api`).

## Configurer Pterodactyl

1. Crée un serveur avec un egg **Node.js 22** et assigne-lui une allocation réseau. Express écoute sur cette allocation via `SERVER_PORT` (ou `PORT`) et sur `0.0.0.0`.
2. Mets la commande de démarrage suivante dans l'onglet **Startup**. Elle installe Express au premier démarrage puis lance le serveur :

   ```sh
   if [ ! -d node_modules/express ]; then npm ci --omit=dev || exit $?; fi; npm start
   ```

3. Ajoute/configure les variables suivantes dans l'onglet **Startup**. Mets un secret long pour `API_TOKEN`; ne le mets pas dans le code d'un site web public.

   | Variable | Valeur conseillée | Rôle |
   | --- | --- | --- |
   | `SERVER_PORT` | Port d'allocation Pterodactyl | Port public d'Express; souvent fourni par l'egg |
   | `HOST` | `0.0.0.0` | Adresse d'écoute Express |
   | `RUST_API_PORT` | `18081` | Port privé du backend; doit être différent de `SERVER_PORT` |
   | `API_TOKEN` | Secret aléatoire | Token Bearer pour `/api/v1/*`; `/healthz` reste public |
   | `API_CORS_ORIGINS` | `https://ton-site.example` | Origines web autorisées, séparées par des virgules; `*` est possible |
   | `API_RATE_LIMIT_PER_MINUTE` | `60` | Limite par IP et fenêtre de 60 secondes |
   | `TRUST_PROXY` | `false` | Ne passe à `true` que si un reverse proxy de confiance est devant Pterodactyl |
   | `MOVIEBOX_CONFIG_DIR` | `/home/container/data/config` | Configuration/session persistante |
   | `MOVIEBOX_DATA_DIR` | `/home/container/data` | Journaux et données persistantes |
   | `MOVIEBOX_CACHE_DIR` | `/home/container/cache` | Cache persistant |

   `MOVIEBOX_API_BIN` est facultatif; il permet de spécifier le chemin complet du binaire si tu ne l'as pas placé dans `bin/moviebox-api`. Ne publie pas `RUST_API_PORT` dans les allocations : le backend est lié à `127.0.0.1`.

4. Démarre le serveur. Dans la console, tu dois voir un message du type `Express API listening on http://0.0.0.0:<port>` puis `Rust provider backend listening privately on 127.0.0.1:<port>`.
5. Vérifie depuis la console ou une machine autorisée :

   ```sh
   curl http://IP_DU_SERVEUR:PORT/healthz
   curl -H "Authorization: Bearer TON_API_TOKEN" \
     'http://IP_DU_SERVEUR:PORT/api/v1/search?provider=moviebox&q=Interstellar'
   ```

Si tu utilises un domaine avec HTTPS, le reverse proxy et le DNS se configurent sur l'hôte Pterodactyl ou auprès de son administrateur; ce dépôt ne modifie pas la configuration globale de Wings/Nginx.

## Sécurité et limites

- Si `API_TOKEN` est vide, les routes de données sont publiques. Le token d'exemple est refusé au démarrage; remplace-le dans les variables Pterodactyl.
- L'API est une façade de recherche/métadonnées autour des fournisseurs. Elle n'enregistre ni ne sert de films. La disponibilité et les limites dépendent des sites amont; les fournisseurs BDIX peuvent nécessiter un réseau compatible.
- L'API n'expose actuellement ni URL de stream, ni proxy vidéo, ni téléchargements, ni comptes utilisateurs. Une application navigateur ne peut pas garder un token partagé secret si celui-ci est intégré à son JavaScript.
- `TRUST_PROXY=true` fait confiance aux en-têtes d'adresse transmis par le reverse proxy. Ne l'active pas si les clients peuvent atteindre Express sans passer par ce proxy.
- Respecte les conditions des fournisseurs et les lois applicables avant d'exposer le service publiquement.
