# Leads to ask for a review. Terms descend in weight; the weights are a starting point to tune.
{
  path = "/home/v/s/g/rolodex/reviews";
  half_life = "60d"; # residence and business move slowly
  tags = {
    business = { type = "bool"; about = "runs a business of their own"; };
    last_login = { type = "timestamp"; };
    interest = { type = "number"; min = 0; max = 1; about = "degree of interest shown"; };
    birthday = { type = "birthday"; about = "when they were born, off an age or a birth year they stated"; };
    lives_in = { type = "place"; };
  };
  rank = [
    { of = "business"; weight = 7; }
    { of = "interactions"; weight = 6; }
    { of = "last_interaction"; decay = 3; weight = 5; }
    { of = "last_login"; decay = 3; weight = 4; }
    { of = "interest"; weight = 3; }
    { of = "birthday"; within = [ 12 26 ]; weight = 2; }
    # the business location
    { of = "lives_in"; near = { lat = 48.8566; lon = 2.3522; radius_km = 30; halving_km = 50; }; weight = 1; }
  ];
}
