--
-- PostgreSQL database dump
--

\restrict IdsfQWgUdLbVSAam0fxpfdih9a8Xz3EfCLdH0tsLcdpJ3FSlo0snAS3aLSHlS2t

-- Dumped from database version 16.13 (Ubuntu 16.13-0ubuntu0.24.04.1)
-- Dumped by pg_dump version 16.13 (Ubuntu 16.13-0ubuntu0.24.04.1)

SET statement_timeout = 0;
SET lock_timeout = 0;
SET idle_in_transaction_session_timeout = 0;
SET client_encoding = 'UTF8';
SET standard_conforming_strings = on;
SELECT pg_catalog.set_config('search_path', '', false);
SET check_function_bodies = false;
SET xmloption = content;
SET client_min_messages = warning;
SET row_security = off;

--
-- Data for Name: onchain_arbs; Type: TABLE DATA; Schema: public; Owner: postgres
--

INSERT INTO public.onchain_arbs VALUES (1, 'UbgXS6Ruy5WDfpD3w91EaDpA3sMj7Ye7D3WNGj6HGnuYnry4SfCGpNzoNnMrB53XxxNYwm8VdaFR9JAfS88bDzC', 407029517, 1773756839, '{2ZeRYwypW8KuE4eXcbFweWeqL2iA1TzdEn6x9j2Uu6yo,GS4CU59F31iL7aR2Q8zVS8DRrcRnXX1yjQ66TqNVQnaR}', '{pumpswap}', 2, false, 14331847, '2ZeRYwypW8KuE4eXcbFweWeqL2iA1TzdEn6x9j2Uu6yo_GS4CU59F31iL7aR2Q8zVS8DRrcRnXX1yjQ66TqNVQnaR', 14, '2026-03-17 15:17:11.298909+01');
INSERT INTO public.onchain_arbs VALUES (2, '42H5tFqPsr3pEKYimy3EdxoK2twYMrfsaQAkLK77MSv73M8pgU8BH7sUt7g6ytZf7eMkE2Snf8yemU1zdz7vy85J', 407029786, 1773756945, '{GS4CU59F31iL7aR2Q8zVS8DRrcRnXX1yjQ66TqNVQnaR}', '{pumpswap}', 2, false, 306302703, 'GS4CU59F31iL7aR2Q8zVS8DRrcRnXX1yjQ66TqNVQnaR', 14, '2026-03-17 15:17:11.310336+01');
INSERT INTO public.onchain_arbs VALUES (3, '3zaMK6NAnyPLwAjg7XxCycr8ee1FMKAf8HyMfYw4RfQioZ7rRREf2gGUVF32HNBxR7PLkg6nWZXt3PzRCSjsuk4D', 407031564, 1773757645, '{9Lo7gMZN6ST8TcVuP9Tz2UMarqrNnzBcz4vQzngQwwia,GS4CU59F31iL7aR2Q8zVS8DRrcRnXX1yjQ66TqNVQnaR}', '{pumpswap}', 2, false, 523079, '9Lo7gMZN6ST8TcVuP9Tz2UMarqrNnzBcz4vQzngQwwia_GS4CU59F31iL7aR2Q8zVS8DRrcRnXX1yjQ66TqNVQnaR', 14, '2026-03-17 15:28:01.001446+01');
INSERT INTO public.onchain_arbs VALUES (4, '4GXNN9yGdoB8wRRs9BS6cgKf6CYSa3RxHt2s5sWJ4mUfe5FfvhFa7NLWcTSZwj1Yz9LscwfmQfj5gh6ApBAtySXc', 407031873, 1773757768, '{GS4CU59F31iL7aR2Q8zVS8DRrcRnXX1yjQ66TqNVQnaR}', '{pumpswap}', 2, false, 65232939, 'GS4CU59F31iL7aR2Q8zVS8DRrcRnXX1yjQ66TqNVQnaR', 14, '2026-03-17 15:30:53.707219+01');
INSERT INTO public.onchain_arbs VALUES (5, '2iyxTct5fbFTh5FuV4cv9V625B9gsQ9pu5Rqoj9Kn5R8pcSAxAM1g4TywdbJmyb5QLLJ2tH55tomnVRm1Q6wg6m3', 407032026, 1773757829, '{TokenkegQfeZyiNwAJbNbGKPFXCWuBvf9Ss623VQ5DA}', '{raydium_v4,orca}', 2, false, 8524, 'TokenkegQfeZyiNwAJbNbGKPFXCWuBvf9Ss623VQ5DA', 14, '2026-03-17 15:30:53.708791+01');


--
-- Name: onchain_arbs_id_seq; Type: SEQUENCE SET; Schema: public; Owner: postgres
--

SELECT pg_catalog.setval('public.onchain_arbs_id_seq', 5, true);


--
-- PostgreSQL database dump complete
--

\unrestrict IdsfQWgUdLbVSAam0fxpfdih9a8Xz3EfCLdH0tsLcdpJ3FSlo0snAS3aLSHlS2t

